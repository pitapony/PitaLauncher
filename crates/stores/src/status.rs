//! The store status row and the unified update summary.
//!
//! The GUI's Stores screen and the headless `pitagames-cli stores` diagnostic
//! both render this: for each CLI store, whether its program is on `PATH`,
//! which version it is, whether the account still has a session, and how many
//! games it sees. Steam is deliberately absent — it needs no CLI and no
//! session, and its scan is [`crate::steam`].
//!
//! The second half is the single list of games with a pending update. Each
//! store reports that differently (Steam writes it into the manifest's
//! `StateFlags`, Epic prints a `needs_verification` flag on its install
//! listing), so [`updates`] is the one place that knows both shapes; a caller
//! never has to.

use std::collections::HashMap;

use pitagames_core::{Game, GameId, Store};
use serde::Serialize;

use crate::acf::InstallPhase;
use crate::amazon::Amazon;
use crate::cli::{CliOptions, StoreStatus};
use crate::epic::Epic;
use crate::gog::Gog;

/// The tag a CLI backend puts on an installed game when the store says the
/// files need verifying before an update (Epic's `needs_verification`).
pub const UPDATE_TAG: &str = "needs-verification";

/// Whether a CLI store has a usable session.
///
/// Three values, not two: "the CLI answered and there is no session" is the
/// actionable one (offer re-auth), while "the CLI never answered" is a probe
/// failure and must not be presented as a signed-out account. `account` is the
/// label the CLI printed; it is empty when the store (GOG) has no account label
/// to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SessionState {
    /// A stored session, for the account label the CLI printed.
    SignedIn { account: String },
    /// The CLI ran and reported no stored session; the user must sign in again.
    SignedOut,
    /// The check did not run, or did not answer.
    Unknown,
}

impl SessionState {
    pub const fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. })
    }

    pub const fn is_signed_out(&self) -> bool {
        matches!(self, Self::SignedOut)
    }

    /// The account label, when there is one.
    pub fn account(&self) -> Option<&str> {
        match self {
            Self::SignedIn { account } if !account.trim().is_empty() => Some(account),
            _ => None,
        }
    }
}

/// One program a store row needs, and what probing it found.
///
/// Most stores have exactly one; GOG has two (`lgogdownloader` for the library
/// and `gogdl` for games), and either being missing disables installs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliTool {
    /// The program as it was asked for (a bare name, or the path from
    /// `--program`).
    pub program: String,
    /// Whether it resolved to an executable, without running it.
    pub status: StoreStatus,
    /// The version it printed, when the run answered and reported one.
    pub version: Option<String>,
}

impl CliTool {
    /// Probes a program name without running it (the cheap half).
    pub fn probe(program: impl Into<String>) -> Self {
        let program = program.into();
        Self {
            status: StoreStatus::probe(&program, None),
            program,
            version: None,
        }
    }

    pub const fn is_ready(&self) -> bool {
        self.status.is_ready()
    }

    /// The resolved executable, when there is one.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.status.path()
    }
}

/// What a store row should highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RowState {
    /// At least one program is missing; listing and installs are unavailable.
    CliMissing,
    /// The programs are there but no session is stored; re-auth is needed.
    SignedOut,
    /// The programs are there but the session check did not answer.
    Unverified,
    /// Ready to list the library and install games.
    Ready,
}

/// One store's status row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreStatusReport {
    pub store: Store,
    /// The programs this store needs, in the order the row shows them.
    pub tools: Vec<CliTool>,
    pub session: SessionState,
    /// Installed games the store reports, when a probe supplied a count.
    pub installed: Option<usize>,
    /// Owned games the store reports, when a probe supplied a count.
    pub owned: Option<usize>,
    /// Non-fatal probe failures, one message each (the row still renders).
    pub problems: Vec<String>,
}

impl StoreStatusReport {
    fn new(store: Store, tools: Vec<CliTool>) -> Self {
        Self {
            store,
            tools,
            session: SessionState::Unknown,
            installed: None,
            owned: None,
            problems: Vec::new(),
        }
    }

    /// Epic's row: one tool, `legendary`.
    ///
    /// `legendary status --offline` carries the game counts, so a plain probe
    /// already fills `owned`/`installed`; `list` refreshes them from the two
    /// listings instead.
    pub fn epic(backend: &Epic, options: &CliOptions, list: bool) -> Self {
        let mut report = Self::new(Store::Epic, vec![CliTool::probe(backend.program())]);
        if !report.tools[0].is_ready() {
            return report;
        }

        match backend.version(options) {
            Ok(version) => report.tools[0].version = version,
            Err(error) => report.problems.push(format!("version: {error}")),
        }
        match backend.account(options) {
            Ok(account) if account.logged_in() => {
                report.session = SessionState::SignedIn {
                    account: account.account,
                };
                report.installed = Some(account.games_installed);
                report.owned = Some(account.games_available);
            }
            Ok(_) => report.session = SessionState::SignedOut,
            Err(error) => report.problems.push(format!("account: {error}")),
        }

        if list && report.session.is_signed_in() {
            report.counts(
                backend
                    .installed(options)
                    .map(|games| games.len())
                    .map_err(|error| error.to_string()),
                backend
                    .owned(options)
                    .map(|games| games.len())
                    .map_err(|error| error.to_string()),
            );
        }

        report
    }

    /// Amazon's row: one tool, `nile`.
    ///
    /// `auth --status` has no counts, so `owned`/`installed` stay `None`
    /// unless the caller asks for the listings with `list`.
    pub fn amazon(backend: &Amazon, options: &CliOptions, list: bool) -> Self {
        let mut report = Self::new(Store::Amazon, vec![CliTool::probe(backend.program())]);
        if !report.tools[0].is_ready() {
            return report;
        }

        match backend.version(options) {
            Ok(version) => report.tools[0].version = version,
            Err(error) => report.problems.push(format!("version: {error}")),
        }
        match backend.account(options) {
            Ok(account) if account.signed_in() => {
                // nile prints the username as `<not logged in>` when signed out;
                // an empty label still means a session exists.
                report.session = SessionState::SignedIn {
                    account: account.username.trim().to_string(),
                };
            }
            Ok(_) => report.session = SessionState::SignedOut,
            Err(error) => report.problems.push(format!("account: {error}")),
        }

        if list && report.session.is_signed_in() {
            report.counts(
                backend
                    .installed(options)
                    .map(|games| games.len())
                    .map_err(|error| error.to_string()),
                backend
                    .owned(options)
                    .map(|games| games.len())
                    .map_err(|error| error.to_string()),
            );
        }

        report
    }

    /// GOG's row: two tools, `lgogdownloader` for the library and `gogdl` for
    /// games. `gogdl` has no installed listing, so `installed` stays `None`.
    pub fn gog(backend: &Gog, options: &CliOptions, list: bool) -> Self {
        let mut report = Self::new(
            Store::Gog,
            vec![
                CliTool::probe(&backend.library_spec().program),
                CliTool::probe(&backend.game_spec().program),
            ],
        );
        if !report.tools[0].is_ready() {
            return report;
        }

        match backend.version(options) {
            Ok(version) => report.tools[0].version = version,
            Err(error) => report.problems.push(format!("version: {error}")),
        }
        match backend.account(options) {
            Ok(account) if account.signed_in() => {
                // `--check-login-status` reports a bool, never a username.
                report.session = SessionState::SignedIn {
                    account: String::new(),
                };
            }
            Ok(_) => report.session = SessionState::SignedOut,
            Err(error) => report.problems.push(format!("account: {error}")),
        }

        if list && report.session.is_signed_in() {
            report.counts(
                Ok(report.installed.unwrap_or(0)),
                backend
                    .owned(options)
                    .map(|games| games.len())
                    .map_err(|error| error.to_string()),
            );
            // The library CLI has no per-game install listing, so `installed`
            // is never known: leave it `None` rather than claim zero.
            report.installed = None;
        }

        report
    }

    fn counts(&mut self, installed: Result<usize, String>, owned: Result<usize, String>) {
        match installed {
            Ok(count) => self.installed = Some(count),
            Err(error) => self.problems.push(format!("installed: {error}")),
        }
        match owned {
            Ok(count) => self.owned = Some(count),
            Err(error) => self.problems.push(format!("owned: {error}")),
        }
    }

    /// Every required program resolved to an executable.
    pub fn is_ready(&self) -> bool {
        !self.tools.is_empty() && self.tools.iter().all(CliTool::is_ready)
    }

    /// Whether the session probe failed rather than answering "signed out".
    pub fn is_unverified(&self) -> bool {
        self.session == SessionState::Unknown
    }

    /// The one value a store row branches on.
    pub fn state(&self) -> RowState {
        if !self.is_ready() {
            return RowState::CliMissing;
        }
        match self.session {
            SessionState::SignedIn { .. } => RowState::Ready,
            SessionState::SignedOut => RowState::SignedOut,
            SessionState::Unknown => RowState::Unverified,
        }
    }

    /// Games can be listed and installed.
    pub fn can_install(&self) -> bool {
        self.state() == RowState::Ready
    }

    /// A store that needs the user to do something before installs work.
    pub fn needs_attention(&self) -> bool {
        self.state() != RowState::Ready
    }

    /// The program names that are missing, for the "install this" line.
    pub fn missing_programs(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|tool| !tool.is_ready())
            .map(|tool| tool.program.as_str())
            .collect()
    }
}

/// Probes the three CLI stores with their default programs, in a fixed order.
///
/// `list` asks for the live game counts, which costs extra CLI runs; a plain
/// status probe only checks `PATH` and the session.
pub fn stores(options: &CliOptions, list: bool) -> Vec<StoreStatusReport> {
    vec![
        StoreStatusReport::epic(&Epic::new(), options, list),
        StoreStatusReport::amazon(&Amazon::new(), options, list),
        StoreStatusReport::gog(&Gog::new(), options, list),
    ]
}

/// Why a game is on the update list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum UpdateSource {
    /// Steam's `appmanifest` says an update is queued or downloading.
    SteamManifest,
    /// The store's own install listing flags the files ([`UPDATE_TAG`]).
    StoreFlag,
}

impl UpdateSource {
    /// A short label for a row or a log line.
    pub const fn label(self) -> &'static str {
        match self {
            Self::SteamManifest => "steam manifest",
            Self::StoreFlag => "store flag",
        }
    }
}

/// An installed game with a pending update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateAvailable {
    pub id: GameId,
    pub title: String,
    pub source: UpdateSource,
    /// The installed version, when the scanner recorded one.
    pub version: Option<String>,
}

/// The unified "update available" list.
///
/// Steam keeps update state in the manifest (`StateFlags` → [`InstallPhase`]),
/// not on the game, so the caller passes the phase map the scan produced; the
/// CLI stores put the flag in a tag. A Steam manifest wins when a game somehow
/// carries both, and the list is sorted by title so it is stable across runs.
///
/// A store that exposes no update signal (Amazon, GOG — neither prints a
/// latest-version field) is absent here rather than reported as up to date.
pub fn updates(games: &[Game], phases: &HashMap<String, InstallPhase>) -> Vec<UpdateAvailable> {
    let mut found = Vec::new();
    for game in games {
        if !game.installed {
            continue;
        }

        let source = if game.id.store == Store::Steam
            && phases.get(&game.id.store_id) == Some(&InstallPhase::UpdateAvailable)
        {
            Some(UpdateSource::SteamManifest)
        } else if game.tags.iter().any(|tag| tag == UPDATE_TAG) {
            Some(UpdateSource::StoreFlag)
        } else {
            None
        };

        if let Some(source) = source {
            found.push(UpdateAvailable {
                id: game.id.clone(),
                title: game.title.clone(),
                source,
                version: game.version.clone(),
            });
        }
    }

    found.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    const LEGENDARY_STATUS: &str = include_str!("../../../fixtures/legendary/status.json");
    const LEGENDARY_LIST: &str = include_str!("../../../fixtures/legendary/list.json");
    const LEGENDARY_INSTALLED: &str =
        include_str!("../../../fixtures/legendary/list-installed.json");
    const NILE_AUTH: &str = include_str!("../../../fixtures/nile/auth-status.json");
    const NILE_LIBRARY: &str = include_str!("../../../fixtures/nile/library-list.json");
    const NILE_INSTALLED: &str = include_str!("../../../fixtures/nile/library-installed.txt");
    const GOG_LIBRARY: &str = include_str!("../../../fixtures/lgogdownloader/list.json");

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

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    /// A fake `legendary` that answers the three probes from the fixtures.
    fn legendary(dir: &Path) -> PathBuf {
        let status = write(dir, "status.json", LEGENDARY_STATUS);
        let list = write(dir, "list.json", LEGENDARY_LIST);
        let installed = write(dir, "list-installed.json", LEGENDARY_INSTALLED);
        script(
            dir,
            "legendary",
            &format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 --version) printf 'legendary version \"0.20.35\", codename \"Pancakes\"\\n' ;;\n\
                 status) cat '{status}' ;;\n\
                 list-installed) cat '{installed}' ;;\n\
                 list) cat '{list}' ;;\n\
                 *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                 esac\n",
                status = status.display(),
                installed = installed.display(),
                list = list.display(),
            ),
        )
    }

    fn options() -> CliOptions {
        CliOptions {
            log_dir: None,
            ..CliOptions::default()
        }
    }

    #[test]
    fn a_missing_program_is_the_only_thing_a_present_less_row_reports() {
        let report = StoreStatusReport::new(
            Store::Epic,
            vec![CliTool::probe("no-such-cli-xyz-anywhere")],
        );
        assert!(!report.is_ready());
        assert_eq!(report.state(), RowState::CliMissing);
        assert_eq!(report.missing_programs(), ["no-such-cli-xyz-anywhere"]);
        // No CLI means no probe ran, so nothing is claimed about the session.
        assert!(!report.can_install());
        assert!(report.needs_attention());
    }

    #[test]
    fn epic_status_reads_version_session_and_the_counts_it_prints() {
        let dir = tempfile::tempdir().unwrap();
        let epic = Epic::with_program(legendary(dir.path()).to_str().unwrap());

        let report = StoreStatusReport::epic(&epic, &options(), false);
        assert_eq!(report.store, Store::Epic);
        assert_eq!(report.tools.len(), 1);
        assert_eq!(
            report.tools[0].program,
            legendary(dir.path()).to_str().unwrap()
        );
        assert!(report.tools[0].is_ready());
        assert_eq!(report.tools[0].version.as_deref(), Some("0.20.35"));
        assert_eq!(
            report.session,
            SessionState::SignedIn {
                account: "player".into()
            }
        );
        // `legendary status --offline` carries the counts, no listing needed.
        assert_eq!(report.installed, Some(2));
        assert_eq!(report.owned, Some(42));
        assert_eq!(report.state(), RowState::Ready);
        assert!(report.can_install());
        assert!(report.problems.is_empty());
    }

    #[test]
    fn list_refreshes_the_counts_from_the_listings() {
        let dir = tempfile::tempdir().unwrap();
        let epic = Epic::with_program(legendary(dir.path()).to_str().unwrap());

        let report = StoreStatusReport::epic(&epic, &options(), true);
        // The fixtures list one base game owned and one installed (DLC dropped).
        assert_eq!(report.owned, Some(2));
        assert_eq!(report.installed, Some(1));
    }

    #[test]
    fn a_signed_out_store_asks_for_re_auth_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let status = write(
            dir.path(),
            "signed-out.json",
            r#"{"account": "<not logged in>", "games_available": 0, "games_installed": 0}"#,
        );
        let program = script(
            dir.path(),
            "legendary",
            &format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 --version) printf 'legendary version \"0.20.35\"\\n' ;;\n\
                 status) cat '{status}' ;;\n\
                 *) exit 2 ;;\n\
                 esac\n",
                status = status.display()
            ),
        );
        let epic = Epic::with_program(program.to_str().unwrap());

        let report = StoreStatusReport::epic(&epic, &options(), false);
        assert_eq!(report.session, SessionState::SignedOut);
        assert_eq!(report.state(), RowState::SignedOut);
        assert!(!report.can_install());
        assert!(report.needs_attention());
        // A missing session is not a probe failure.
        assert!(report.problems.is_empty());
    }

    #[test]
    fn a_probe_that_never_answers_is_unverified_not_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        // A CLI that exists but errors on `status` leaves the session unknown.
        let program = script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             case \"$1\" in\n\
             --version) printf 'legendary version \"1.0\"\\n' ;;\n\
             status) echo 'boom' >&2; exit 1 ;;\n\
             *) exit 2 ;;\n\
             esac\n",
        );
        let epic = Epic::with_program(program.to_str().unwrap());

        let report = StoreStatusReport::epic(&epic, &options(), false);
        assert_eq!(report.state(), RowState::Unverified);
        assert!(report.is_unverified());
        assert!(!report.can_install());
        // The failure is recorded rather than silently shrinking the row.
        assert_eq!(report.problems.len(), 1);
        assert!(
            report.problems[0].contains("account:"),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn amazon_reports_its_session_and_counts_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let auth = write(dir.path(), "auth.json", NILE_AUTH);
        let library = write(dir.path(), "library.json", NILE_LIBRARY);
        let installed = write(dir.path(), "installed.txt", NILE_INSTALLED);
        let nile = script(
            dir.path(),
            "nile",
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then printf '1.2.0 Robert Speedwagon\\n'; exit 0; fi\n\
                 if [ \"$1\" = \"auth\" ]; then cat '{auth}'; exit 0; fi\n\
                 if [ \"$1\" = \"library\" ]; then\n\
                   if [ \"$3\" = \"--installed\" ]; then cat '{installed}'; exit 0; fi\n\
                   cat '{library}'; exit 0\n\
                 fi\n\
                 echo \"unexpected: $*\" >&2; exit 2\n",
                auth = auth.display(),
                library = library.display(),
                installed = installed.display(),
            ),
        );
        let amazon = Amazon::with_program(nile.to_str().unwrap());

        let plain = StoreStatusReport::amazon(&amazon, &options(), false);
        assert_eq!(plain.store, Store::Amazon);
        assert_eq!(plain.tools[0].version.as_deref(), Some("1.2.0"));
        assert_eq!(
            plain.session,
            SessionState::SignedIn {
                account: "player".into()
            }
        );
        assert_eq!(plain.state(), RowState::Ready);
        // `auth --status` carries no counts, so a plain probe claims none.
        assert_eq!(plain.owned, None);
        assert_eq!(plain.installed, None);

        let listed = StoreStatusReport::amazon(&amazon, &options(), true);
        assert_eq!(listed.owned, Some(2));
        assert_eq!(listed.installed, Some(2));
    }

    #[test]
    fn gog_needs_both_programs_and_names_the_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let lgog = script(dir.path(), "lgogdownloader", "#!/bin/sh\nexit 0\n");

        let gog = Gog::with_programs(lgog.to_str().unwrap(), "no-such-gogdl-xyz");
        let report = StoreStatusReport::gog(&gog, &options(), false);
        assert_eq!(report.store, Store::Gog);
        assert_eq!(report.tools.len(), 2);
        assert!(report.tools[0].is_ready());
        assert!(!report.tools[1].is_ready());
        assert_eq!(report.state(), RowState::CliMissing);
        assert_eq!(report.missing_programs(), ["no-such-gogdl-xyz"]);
    }

    #[test]
    fn gog_never_claims_an_installed_count() {
        let dir = tempfile::tempdir().unwrap();
        let lgog = script(
            dir.path(),
            "lgogdownloader",
            &format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 --version) printf 'LGOGDownloader 3.18\\n' ;;\n\
                 --check-login-status) printf 'Login status: Logged in\\n' ;;\n\
                 --list) cat '{library}' ;;\n\
                 *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                 esac\n",
                library = write(dir.path(), "library.json", GOG_LIBRARY).display()
            ),
        );
        let gogdl = script(dir.path(), "gogdl", "#!/bin/sh\nexit 0\n");
        let gog = Gog::with_programs(lgog.to_str().unwrap(), gogdl.to_str().unwrap());

        let report = StoreStatusReport::gog(&gog, &options(), true);
        assert_eq!(report.tools[0].version.as_deref(), Some("3.18"));
        assert!(report.session.is_signed_in());
        // `--check-login-status` prints no username, so there is no label.
        assert_eq!(report.session.account(), None);
        // The GOG library CLI has no per-game installed listing at all.
        assert_eq!(report.installed, None);
        assert_eq!(report.owned, Some(3));
    }

    #[test]
    fn updates_unify_steam_manifests_and_store_flags() {
        let mut steam = Game::new(GameId::new(Store::Steam, "620"), "Portal 2");
        steam.installed = true;
        let mut dota = Game::new(GameId::new(Store::Steam, "570"), "Dota 2");
        dota.installed = true;

        let mut epic = Game::new(GameId::new(Store::Epic, "Gizmo"), "Gizmo");
        epic.installed = true;
        epic.version = Some("0.9.1".into());
        epic.tags = vec!["Windows".into(), UPDATE_TAG.into()];

        // A not-installed game with the flag is not an update.
        let mut owned = Game::new(GameId::new(Store::Epic, "Sugar"), "Sugar");
        owned.tags = vec![UPDATE_TAG.into()];

        let games = vec![steam, dota, epic, owned];
        let mut phases = HashMap::new();
        phases.insert("620".to_string(), InstallPhase::UpdateAvailable);
        phases.insert("570".to_string(), InstallPhase::Installed);

        let found = updates(&games, &phases);
        let ids: Vec<&str> = found.iter().map(|u| u.id.store_id.as_str()).collect();
        assert_eq!(ids, ["Gizmo", "620"]);
        assert_eq!(found[0].source, UpdateSource::StoreFlag);
        assert_eq!(found[1].source, UpdateSource::SteamManifest);
        assert_eq!(found[0].version.as_deref(), Some("0.9.1"));
    }

    #[test]
    fn a_steam_manifest_wins_over_a_stale_tag() {
        let mut game = Game::new(GameId::new(Store::Steam, "620"), "Portal 2");
        game.installed = true;
        game.tags = vec![UPDATE_TAG.into()];

        let mut phases = HashMap::new();
        phases.insert("620".to_string(), InstallPhase::UpdateAvailable);

        let found = updates(&[game], &phases);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, UpdateSource::SteamManifest);
    }

    #[test]
    fn an_empty_library_is_an_empty_update_list() {
        assert!(updates(&[], &HashMap::new()).is_empty());
    }

    #[test]
    fn stores_probes_all_three_in_a_fixed_order() {
        let reports = stores(&options(), false);
        let names: Vec<&str> = reports.iter().map(|r| r.store.as_str()).collect();
        assert_eq!(names, ["epic", "amazon", "gog"]);
        // Every store row names at least one program, whatever `PATH` holds.
        assert!(reports.iter().all(|r| !r.tools.is_empty()));
        // GOG's second program is `gogdl`; Epic and Amazon have one each.
        assert_eq!(reports[0].tools.len(), 1);
        assert_eq!(reports[1].tools.len(), 1);
        assert_eq!(reports[2].tools.len(), 2);
    }
}
