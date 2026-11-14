//! The CLI stores' libraries, as rows a cache can hold.
//!
//! Each backend can list two different things: what the account **owns**
//! (`legendary list --json`, `nile library list --json`, `lgogdownloader --list
//! json`) and what is **installed on this machine** (`legendary list-installed
//! --json`, `nile library list --installed`; GOG has no such listing at all).
//! Neither alone is a library: the owned list has titles for games that are not
//! downloaded, and the installed list has the path, the size and the version.
//!
//! This module folds the two into [`Game`] rows and reconciles them with what
//! the cache already holds. It is deliberately pure and backend-driven — no
//! SQLite, no Dioxus — exactly like [`crate::status`]: the database write is a
//! separate step (`pitagames_db::GameRepository::replace_store_games`), and the
//! GUI and `pitagames-cli stores sync` are two callers of the same model.
//!
//! ```text
//!  Backends::library(store) ──► StoreLibrary { installed, owned, flags }
//!          │                             │
//!          ├─ legend InstalledEntry       ├─ rows(): the two listings folded
//!          ├─ nile text + installed.json  └─ merge(cached): cache-aware rows
//!          └─ lgogdownloader details
//! ```
//!
//! **The one rule that matters is that a listing failure must never delete
//! rows.** A store CLI that logs an error and exits `0` (legendary, nile) can
//! answer one listing and fail the other; an installed game whose install
//! listing failed must not be rewritten as "not installed" — that would drop
//! its path and its size — and a store whose owned listing failed must not be
//! written at all, because an empty answer would delete every row it owns.
//! [`StoreLibrary::can_replace`] and [`merge`] are that policy, and both are
//! covered by tests.

use pitagames_core::{Game, OwnedGame, Store};
use serde::Serialize;

use crate::amazon::Amazon;
use crate::cli::CliOptions;
use crate::epic::Epic;
use crate::gog::Gog;
use crate::status::CLI_STORES;

/// What one store's command-line clients said about its library.
///
/// The two listings answer independently, so each carries its own flag: a
/// failure is a warning on the report, not an error that aborts the sync.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreLibrary {
    pub store: Store,
    /// Rows from the install listing (`installed = true`, with paths).
    pub installed: Vec<Game>,
    /// Everything the account owns, installed or not.
    pub owned: Vec<OwnedGame>,
    /// The install listing answered. GOG has no such listing, so this stays
    /// false there by design rather than as a failure.
    pub installed_known: bool,
    /// The owned listing answered. Without it there is no library at all, so a
    /// sync must not write.
    pub owned_known: bool,
    /// Listing failures, one message each (the sync still runs the other one).
    pub warnings: Vec<String>,
}

impl StoreLibrary {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            installed: Vec::new(),
            owned: Vec::new(),
            installed_known: false,
            owned_known: false,
            warnings: Vec::new(),
        }
    }

    /// Both listings the store can answer answered, so the cache can be
    /// replaced. A half-answer would delete rows the store still owns.
    pub fn can_replace(&self) -> bool {
        self.owned_known
    }

    /// The two listings folded into one row per game.
    ///
    /// An installed row wins over its owned twin — it has the path, the size
    /// and the version — and only borrows playtime/metadata the install listing
    /// never carries. An owned game with no installed row becomes a
    /// `not installed` row so the grid offers Install for it. Sorted by title.
    pub fn rows(&self) -> Vec<Game> {
        let mut rows = self.installed.clone();
        for owned in &self.owned {
            if let Some(existing) = rows.iter_mut().find(|game| game.id == owned.id) {
                if existing.playtime.is_zero() {
                    existing.playtime = owned.playtime;
                }
                if existing.last_played.is_none() {
                    existing.last_played = owned.last_played;
                }
                continue;
            }

            let mut game = Game::new(owned.id.clone(), owned.title.clone());
            game.playtime = owned.playtime;
            game.last_played = owned.last_played;
            rows.push(game);
        }

        sort_rows(&mut rows);
        rows
    }

    /// Installed rows in the folded listing.
    pub fn installed_count(&self) -> usize {
        self.rows().iter().filter(|game| game.installed).count()
    }

    /// Rows the account owns but has not installed.
    pub fn owned_only_count(&self) -> usize {
        self.rows().iter().filter(|game| !game.installed).count()
    }
}

/// Merge a store's fresh listing over the rows the cache already holds.
///
/// The rules, in order:
///
/// 1. Everything the store reported is written: installed rows with their
///    paths, and owned games as `not installed` rows.
/// 2. An owned row does **not** count as "uninstalled" when the store's install
///    listing failed ([`StoreLibrary::installed_known`] is false). The cached
///    installed row is kept — a failed listing is not an uninstall.
/// 3. A cached installed row the listing never mentions is kept too. This is
///    the GOG and importer case: `gogdl`/`lgogdownloader` cannot list installs,
///    and Heroic/Lutris/Bottles imports know about trees the store CLI does not.
/// 4. When the store's owned listing failed the whole answer is incomplete, so
///    nothing is dropped: the cached rows come back unchanged.
pub fn merge(cached: &[Game], fresh: &StoreLibrary) -> Vec<Game> {
    let same_store = |row: &&Game| row.id.store == fresh.store;
    let cached: Vec<&Game> = cached.iter().filter(same_store).collect();

    if !fresh.can_replace() {
        let mut rows: Vec<Game> = cached.into_iter().cloned().collect();
        sort_rows(&mut rows);
        return rows;
    }

    let mut rows: Vec<Game> = Vec::new();
    for game in fresh.rows() {
        let cached_row = cached.iter().find(|row| row.id == game.id);
        let keep_cached_install = !game.installed
            && !fresh.installed_known
            && cached_row.is_some_and(|row| row.installed);
        if !keep_cached_install {
            rows.push(game);
        }
    }

    for game in cached.into_iter().filter(|row| row.installed) {
        if !rows.iter().any(|row| row.id == game.id) {
            rows.push(game.clone());
        }
    }

    sort_rows(&mut rows);
    rows
}

fn sort_rows(rows: &mut [Game]) {
    rows.sort_by(|left, right| {
        left.title
            .to_lowercase()
            .cmp(&right.title.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// What one store's sync did, for the CLI's table and `--json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncReport {
    pub store: Store,
    /// Rows the merged listing holds (what the cache now contains).
    pub rows: usize,
    pub installed: usize,
    pub owned_only: usize,
    /// Rows the write removed from the cache; `0` for a dry run.
    pub removed: usize,
    /// Whether the cache was replaced. `false` means the store's answer was
    /// incomplete (or the caller asked for a dry run).
    pub applied: bool,
    pub warnings: Vec<String>,
}

impl SyncReport {
    /// A report for a finished (or planned) sync of one store.
    pub fn new(library: &StoreLibrary, rows: &[Game], removed: usize, applied: bool) -> Self {
        Self {
            store: library.store,
            rows: rows.len(),
            installed: rows.iter().filter(|game| game.installed).count(),
            owned_only: rows.iter().filter(|game| !game.installed).count(),
            removed,
            applied,
            warnings: library.warnings.clone(),
        }
    }
}

/// The Epic library: `list-installed --json` plus `list --json`.
pub fn epic_library(backend: &Epic, options: &CliOptions) -> StoreLibrary {
    let mut library = StoreLibrary::new(Store::Epic);
    match backend.installed(options) {
        Ok(games) => {
            library.installed = games;
            library.installed_known = true;
        }
        Err(error) => library.warnings.push(format!("installed: {error}")),
    }
    match backend.owned(options) {
        Ok(games) => {
            library.owned = games;
            library.owned_known = true;
        }
        Err(error) => library.warnings.push(format!("owned: {error}")),
    }
    library
}

/// The Amazon library: `library list --json` plus `library list --installed`
/// (enriched from nile's own `installed.json`).
pub fn amazon_library(backend: &Amazon, options: &CliOptions) -> StoreLibrary {
    let mut library = StoreLibrary::new(Store::Amazon);
    match backend.installed(options) {
        Ok(games) => {
            library.installed = games;
            library.installed_known = true;
        }
        Err(error) => library.warnings.push(format!("installed: {error}")),
    }
    match backend.owned(options) {
        Ok(games) => {
            library.owned = games;
            library.owned_known = true;
        }
        Err(error) => library.warnings.push(format!("owned: {error}")),
    }
    library
}

/// The GOG library: `lgogdownloader --list json` only.
///
/// `gogdl` has no installed listing and `lgogdownloader --status` checks its own
/// download directory, not a game tree, so this library never claims to know
/// what is installed: [`merge`] keeps whatever the cache (an import, a previous
/// install) recorded. That is why `installed_known` stays false without a
/// warning here.
pub fn gog_library(backend: &Gog, options: &CliOptions) -> StoreLibrary {
    let mut library = StoreLibrary::new(Store::Gog);
    match backend.owned(options) {
        Ok(games) => {
            library.owned = games;
            library.owned_known = true;
        }
        Err(error) => library.warnings.push(format!("owned: {error}")),
    }
    library
}

/// The three CLI backends a library sync drives.
///
/// One struct rather than a trait object: the backends take different program
/// names (GOG has two) and an override must land on exactly one of them, which
/// a plain field cannot mix up. The CLI's `--program`/`--game-program` flags and
/// the GUI's defaults both build this.
#[derive(Debug, Clone)]
pub struct Backends {
    pub epic: Epic,
    pub amazon: Amazon,
    pub gog: Gog,
}

impl Default for Backends {
    fn default() -> Self {
        Self::new()
    }
}

impl Backends {
    /// The default programs (`legendary`, `nile`, `lgogdownloader` + `gogdl`).
    pub fn new() -> Self {
        Self {
            epic: Epic::new(),
            amazon: Amazon::new(),
            gog: Gog::new(),
        }
    }

    /// One store's library, folded but not yet merged with a cache.
    pub fn library(&self, store: Store, options: &CliOptions) -> StoreLibrary {
        match store {
            Store::Epic => epic_library(&self.epic, options),
            Store::Amazon => amazon_library(&self.amazon, options),
            Store::Gog => gog_library(&self.gog, options),
            other => {
                let mut library = StoreLibrary::new(other);
                library.warnings.push(format!(
                    "{other} has no CLI store backend; only epic, amazon and gog sync"
                ));
                library
            }
        }
    }

    /// Every CLI store's library, in the row order the Stores screen uses.
    pub fn all(&self, options: &CliOptions) -> Vec<StoreLibrary> {
        CLI_STORES
            .iter()
            .map(|store| self.library(*store, options))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::GameId;
    use std::fs;
    use std::path::{Path, PathBuf};

    const LEGENDARY_LIST: &str = include_str!("../../../fixtures/legendary/list.json");
    const LEGENDARY_INSTALLED: &str =
        include_str!("../../../fixtures/legendary/list-installed.json");
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

    fn options() -> CliOptions {
        CliOptions {
            log_dir: None,
            ..CliOptions::default()
        }
    }

    fn epic_game(id: &str, title: &str) -> Game {
        let mut game = Game::new(GameId::new(Store::Epic, id), title);
        game.installed = true;
        game
    }

    fn library_from<I>(installed: Vec<Game>, owned: I) -> StoreLibrary
    where
        I: IntoIterator<Item = (&'static str, &'static str)>,
    {
        let mut library = StoreLibrary::new(Store::Epic);
        library.installed = installed;
        library.installed_known = true;
        library.owned = owned
            .into_iter()
            .map(|(id, title)| OwnedGame::new(GameId::new(Store::Epic, id), title))
            .collect();
        library.owned_known = true;
        library
    }

    #[test]
    fn the_folded_listing_prefers_installed_rows_and_sorts_by_title() {
        let mut installed = epic_game("Gizmo", "Gizmo");
        installed.install_dir = Some(PathBuf::from("/games/epic/Gizmo"));
        let library = library_from(
            vec![installed, epic_game("Zed", "zed")],
            [("Gizmo", "Gizmo"), ("Sugar", "Sugar"), ("Other", "Other")],
        );

        let rows = library.rows();
        let ids: Vec<&str> = rows.iter().map(|game| game.id.store_id.as_str()).collect();
        assert_eq!(ids, ["Gizmo", "Other", "Sugar", "Zed"]);

        let gizmo = &rows[0];
        assert!(gizmo.installed, "the install listing wins");
        assert_eq!(
            gizmo.install_dir.as_deref(),
            Some(Path::new("/games/epic/Gizmo"))
        );
        assert!(!rows[1].installed, "an owned game is an Install row");
        assert_eq!(library.installed_count(), 2);
        assert_eq!(library.owned_only_count(), 2);
    }

    #[test]
    fn an_owned_twin_only_fills_what_the_install_listing_lacks() {
        let mut installed = epic_game("Gizmo", "Gizmo");
        installed.playtime = std::time::Duration::from_secs(60);
        let mut library = library_from(vec![installed], [("Gizmo", "Gizmo")]);
        library.owned[0].playtime = std::time::Duration::from_secs(3600);

        let rows = library.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].playtime,
            std::time::Duration::from_secs(60),
            "a known playtime is not overwritten by the owned listing"
        );
    }

    #[test]
    fn a_failed_install_listing_does_not_uninstall_a_cached_game() {
        // legendary answered `list` but failed `list-installed`: Gizmo is owned
        // and (per the cache) installed. It must keep its path.
        let mut library = StoreLibrary::new(Store::Epic);
        library.owned_known = true;
        library.owned = vec![OwnedGame::new(GameId::new(Store::Epic, "Gizmo"), "Gizmo")];
        library.warnings.push("installed: boom".into());

        let mut cached = epic_game("Gizmo", "Gizmo");
        cached.install_dir = Some(PathBuf::from("/games/epic/Gizmo"));
        cached.size_bytes = Some(42);

        let rows = merge(&[cached.clone()], &library);
        assert_eq!(rows, vec![cached]);
        assert!(rows[0].installed);
    }

    #[test]
    fn an_answered_install_listing_brings_an_uninstall_back() {
        // This time the install listing answered and did not name Gizmo: the
        // game really is gone, so the row becomes an Install row.
        let library = library_from(Vec::new(), [("Gizmo", "Gizmo")]);
        let cached = epic_game("Gizmo", "Gizmo");

        let rows = merge(&[cached], &library);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].installed);
        assert!(rows[0].install_dir.is_none());
    }

    #[test]
    fn an_installed_row_the_listing_never_mentions_survives() {
        // GOG: `lgogdownloader` lists ownership only, and an import recorded an
        // installed tree. The sync must not delete it.
        let mut library = StoreLibrary::new(Store::Gog);
        library.owned_known = true;
        library.owned = vec![OwnedGame::new(
            GameId::new(Store::Gog, "1207658930"),
            "Game",
        )];

        let mut imported = Game::new(GameId::new(Store::Gog, "1207658930"), "Game");
        imported.installed = true;
        imported.install_dir = Some(PathBuf::from("/games/gog/Game"));
        let mut other = Game::new(GameId::new(Store::Gog, "999"), "Imported Only");
        other.installed = true;

        let rows = merge(&[imported.clone(), other.clone()], &library);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|game| game.installed));
        assert!(rows.contains(&imported));
        assert!(rows.contains(&other));
        assert!(!library.installed_known, "GOG has no install listing");
    }

    #[test]
    fn an_incomplete_answer_returns_the_cache_untouched() {
        // The owned listing failed, so the answer is empty for the wrong
        // reason: nothing may be dropped.
        let mut library = StoreLibrary::new(Store::Epic);
        library.warnings.push("owned: boom".into());
        assert!(!library.can_replace());

        let cached = vec![epic_game("Gizmo", "Gizmo"), epic_game("Sugar", "Sugar")];
        assert_eq!(merge(&cached, &library), cached);
    }

    #[test]
    fn a_new_owned_game_is_added_and_a_dropped_one_is_gone() {
        let library = library_from(vec![], [("Sugar", "Sugar")]);
        // An owned-only cached row: the store no longer lists it, so it goes.
        let cached = vec![Game::new(GameId::new(Store::Epic, "Gizmo"), "Gizmo")];

        let rows = merge(&cached, &library);
        let ids: Vec<&str> = rows.iter().map(|game| game.id.store_id.as_str()).collect();
        assert_eq!(ids, ["Sugar"]);
        assert!(!rows[0].installed);
    }

    #[test]
    fn rows_are_only_ever_the_books_store_rows() {
        let library = library_from(vec![], [("Sugar", "Sugar")]);
        let mut steam = Game::new(GameId::new(Store::Steam, "620"), "Portal 2");
        steam.installed = true;

        // A Steam row is not this store's business: it neither survives the
        // merge nor blocks the owned row.
        let rows = merge(&[steam], &library);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.store, Store::Epic);
    }

    #[test]
    fn epic_reads_both_listings_through_the_real_parsers() {
        let dir = tempfile::tempdir().unwrap();
        let epic = Epic::with_program(legendary(dir.path()).to_str().unwrap());

        let library = epic_library(&epic, &options());
        assert!(library.can_replace(), "{:?}", library.warnings);
        assert!(library.installed_known);
        assert!(library.warnings.is_empty());
        assert_eq!(library.installed.len(), 1, "one installed base game");
        assert_eq!(library.owned.len(), 2, "two owned base games");

        let rows = library.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id.store, Store::Epic);
        assert!(rows.iter().any(|row| row.installed));
    }

    #[test]
    fn a_signed_out_epic_reports_warnings_and_never_replaces_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             echo '[cli] ERROR: No saved credentials' >&2\n\
             exit 0\n",
        );
        let epic = Epic::with_program(program.to_str().unwrap());

        let library = epic_library(&epic, &options());
        assert!(!library.can_replace());
        assert!(library.rows().is_empty());
        assert!(!library.warnings.is_empty());
    }

    #[test]
    fn amazon_folds_the_text_install_listing_with_the_owned_library() {
        let dir = tempfile::tempdir().unwrap();
        let library = write(dir.path(), "library.json", NILE_LIBRARY);
        let installed = write(dir.path(), "installed.txt", NILE_INSTALLED);
        let config = dir.path().join("nile-config");
        let nile_dir = config.join("nile");
        fs::create_dir_all(&nile_dir).unwrap();
        write(
            &nile_dir,
            "installed.json",
            include_str!("../../../fixtures/nile/installed.json"),
        );
        let nile = script(
            dir.path(),
            "nile",
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"library\" ] && [ \"$3\" = \"--json\" ]; then cat '{library}'; exit 0; fi\n\
                 if [ \"$1\" = \"library\" ] && [ \"$3\" = \"--installed\" ]; then cat '{installed}'; exit 0; fi\n\
                 echo \"unexpected: $*\" >&2; exit 2\n",
                library = library.display(),
                installed = installed.display(),
            ),
        );
        let amazon = Amazon::with_program(nile.to_str().unwrap());

        let _guard = env_lock();
        // nile appends `nile` to the base, so point it at the fixture parent.
        unsafe { std::env::set_var("NILE_CONFIG_PATH", &config) };
        let library = amazon_library(&amazon, &options());
        unsafe { std::env::remove_var("NILE_CONFIG_PATH") };

        assert!(library.can_replace(), "{:?}", library.warnings);
        assert!(library.installed_known);
        assert_eq!(library.installed.len(), 2);
        assert_eq!(library.owned.len(), 2);
    }

    #[test]
    fn gog_reads_the_library_only_and_has_no_install_listing() {
        let dir = tempfile::tempdir().unwrap();
        let lgog = script(
            dir.path(),
            "lgogdownloader",
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--list\" ]; then cat '{}'; exit 0; fi\n\
                 echo \"unexpected: $*\" >&2; exit 2\n",
                write(dir.path(), "library.json", GOG_LIBRARY).display()
            ),
        );
        let gogdl = script(dir.path(), "gogdl", "#!/bin/sh\nexit 0\n");
        let gog = Gog::with_programs(lgog.to_str().unwrap(), gogdl.to_str().unwrap());

        let library = gog_library(&gog, &options());
        assert!(library.can_replace(), "{:?}", library.warnings);
        assert!(!library.installed_known, "GOG cannot list installs");
        assert!(library.warnings.is_empty(), "that is not a failure");
        assert_eq!(library.owned.len(), 3);
        assert!(library.installed.is_empty());
    }

    #[test]
    fn all_three_stores_sync_in_the_row_order_and_fail_together_quietly() {
        // Programs that do not exist: every store reports a warning and none
        // claims a replaceable library. Nothing panics, nothing is written.
        let backends = Backends {
            epic: Epic::with_program("/nonexistent/legendary"),
            amazon: Amazon::with_program("/nonexistent/nile"),
            gog: Gog::with_programs("/nonexistent/lgogdownloader", "/nonexistent/gogdl"),
        };
        let libraries = backends.all(&options());

        let names: Vec<&str> = libraries
            .iter()
            .map(|library| library.store.as_str())
            .collect();
        assert_eq!(names, ["epic", "amazon", "gog"]);
        assert!(libraries.iter().all(|library| !library.can_replace()));
        assert!(libraries.iter().all(|library| !library.warnings.is_empty()));
    }

    #[test]
    fn a_store_with_no_backend_is_refused_not_guessed() {
        let library = Backends::new().library(Store::Local, &options());
        assert!(!library.can_replace());
        assert_eq!(library.warnings.len(), 1);
        assert!(library.warnings[0].contains("local"));
    }

    #[test]
    fn a_report_counts_installed_and_owned_only_rows() {
        let library = library_from(vec![epic_game("Gizmo", "Gizmo")], [("Sugar", "Sugar")]);
        let rows = library.rows();
        let report = SyncReport::new(&library, &rows, 1, true);

        assert_eq!(report.rows, 2);
        assert_eq!(report.installed, 1);
        assert_eq!(report.owned_only, 1);
        assert_eq!(report.removed, 1);
        assert!(report.applied);
        assert_eq!(report.store, Store::Epic);
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// A fake `legendary` that answers the two listings from the fixtures.
    fn legendary(dir: &Path) -> PathBuf {
        let list = write(dir, "list.json", LEGENDARY_LIST);
        let installed = write(dir, "list-installed.json", LEGENDARY_INSTALLED);
        script(
            dir,
            "legendary",
            &format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 list-installed) cat '{installed}' ;;\n\
                 list) cat '{list}' ;;\n\
                 *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                 esac\n",
                installed = installed.display(),
                list = list.display(),
            ),
        )
    }
}
