//! Library presentation state — framework-free and unit-tested.
//!
//! Keeping this layer free of Dioxus types lets the renderer underneath the
//! grid change without dragging scan logic along, and gives the grid plain
//! data that can be tested headlessly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pitagames_art::ArtRequest;
use pitagames_core::Game;
use pitagames_core::launch::{steam_install, steam_run};
use pitagames_db::StoredMetadata;
use pitagames_metadata::CachePolicy;
use pitagames_stores::{InstallPhase, scan_steam};
use time::OffsetDateTime;

/// One row of the launcher grid: the scanned game, Steam's install state and
/// whatever the store metadata cache knows about it.
#[derive(Debug, Clone, PartialEq)]
pub struct GameEntry {
    pub game: Game,
    /// `StateFlags` view of the manifest; `Installed` when Steam wrote none.
    pub phase: InstallPhase,
    /// Cached store metadata, including the negative "no data" answer.
    pub metadata: Option<StoredMetadata>,
}

impl GameEntry {
    /// The store's own id (Steam appid for now).
    pub fn app_id(&self) -> &str {
        &self.game.id.store_id
    }

    /// The store description and art URLs, when the cache has them.
    pub fn store_metadata(&self) -> Option<&pitagames_core::GameMetadata> {
        self.metadata.as_ref()?.metadata.as_ref()
    }

    /// When the cached store answer was received.
    pub fn metadata_fetched_at(&self) -> Option<OffsetDateTime> {
        self.metadata.as_ref().map(|row| row.fetched_at)
    }

    pub fn title(&self) -> &str {
        &self.game.title
    }

    pub fn size_bytes(&self) -> Option<u64> {
        self.game.size_bytes
    }

    pub fn playtime(&self) -> Duration {
        self.game.playtime
    }

    /// Files are present and Steam is not writing them.
    pub fn can_play(&self) -> bool {
        self.game.installed && !self.phase.is_busy()
    }

    /// The install URL applies to both "not installed" and "update waiting".
    pub fn can_install(&self) -> bool {
        matches!(
            self.phase,
            InstallPhase::NotInstalled | InstallPhase::UpdateAvailable
        )
    }

    /// Status shown next to the title; `None` for a plain installed game.
    pub fn status_label(&self) -> Option<&'static str> {
        match self.phase {
            InstallPhase::Installed => None,
            InstallPhase::Unknown => Some("state unknown"),
            other => Some(other.as_str()),
        }
    }

    /// Grid meta line: store, appid, size, playtime when known.
    pub fn meta_line(&self) -> String {
        let mut parts = vec!["steam".to_string(), format!("appid {}", self.app_id())];

        if let Some(bytes) = self.size_bytes() {
            parts.push(crate::format::human_bytes(bytes));
        }
        if !self.game.playtime.is_zero() {
            parts.push(format!(
                "played {}",
                crate::format::playtime(self.playtime())
            ));
        }

        parts.join(" · ")
    }
}

/// The library as loaded, plus warnings from the scan.
#[derive(Debug, Clone, Default)]
pub struct Library {
    pub games: Vec<GameEntry>,
    pub warnings: Vec<String>,
    /// Non-fatal problems from the store metadata refresh.
    pub metadata_warnings: Vec<String>,
    /// True while a background metadata fetch is running.
    pub metadata_pending: bool,
    /// The first Steam root that yielded a library, for the header.
    pub steam_root: Option<PathBuf>,
    /// Every library folder scanned; install trackers poll these.
    pub libraries: Vec<PathBuf>,
}

impl Library {
    /// Scan every Steam root this machine exposes and merge the results.
    pub fn load() -> Self {
        let roots: Vec<PathBuf> = pitagames_core::paths::steam_roots()
            .into_iter()
            .filter(|root| root.join("steamapps").is_dir())
            .collect();

        let mut merged = Self::default();
        for root in roots {
            merged.absorb(Self::scan_root(&root));
        }

        if merged.steam_root.is_none() {
            merged
                .warnings
                .push("No Steam library found on this machine".to_string());
        }

        merged.games.sort_by(|left, right| {
            left.title()
                .to_lowercase()
                .cmp(&right.title().to_lowercase())
                .then_with(|| left.app_id().cmp(right.app_id()))
        });

        merged
    }

    /// Scan one Steam root, in presentation order.
    pub fn scan_root(steam_root: &Path) -> Self {
        let report = scan_steam(steam_root, false);
        let games =
            report
                .games
                .into_iter()
                .map(|game| {
                    // A manifest Steam never wrote flags into is an installed app.
                    let phase = report.phases.get(&game.id.store_id).copied().unwrap_or(
                        if game.installed {
                            InstallPhase::Installed
                        } else {
                            InstallPhase::NotInstalled
                        },
                    );
                    GameEntry {
                        game,
                        phase,
                        metadata: None,
                    }
                })
                .collect();

        Self {
            games,
            warnings: report.warnings,
            metadata_warnings: Vec::new(),
            metadata_pending: false,
            steam_root: Some(steam_root.to_path_buf()),
            libraries: report.libraries,
        }
    }

    /// Merge another root's scan in, without duplicating appids.
    fn absorb(&mut self, other: Self) {
        self.warnings.extend(other.warnings);
        self.metadata_warnings.extend(other.metadata_warnings);
        if self.steam_root.is_none() {
            self.steam_root = other.steam_root;
        }
        for library in other.libraries {
            if !self.libraries.contains(&library) {
                self.libraries.push(library);
            }
        }

        for entry in other.games {
            match self
                .games
                .iter_mut()
                .find(|known| known.app_id() == entry.app_id())
            {
                // The same appid can be listed by two roots that share a library
                // folder; keep the copy whose files are actually present.
                Some(known) if !known.game.installed && entry.game.installed => *known = entry,
                Some(_) => {}
                None => self.games.push(entry),
            }
        }
    }

    /// Filter by a case-insensitive title substring.
    ///
    /// Pre-ordered by title; the filter preserves the order.
    pub fn filtered(&self, query: &str) -> Vec<&GameEntry> {
        let query = query.trim().to_lowercase();
        self.games
            .iter()
            .filter(|entry| query.is_empty() || entry.title().to_lowercase().contains(&query))
            .collect()
    }

    /// Games Steam is currently installing or updating: the downloads view
    /// starts following these as soon as the scan lands.
    pub fn in_flight(&self) -> Vec<&GameEntry> {
        self.games
            .iter()
            .filter(|entry| entry.phase.is_busy())
            .collect()
    }

    /// The scanned games as plain core values, for persistence.
    pub fn snapshot_games(&self) -> Vec<Game> {
        self.games.iter().map(|entry| entry.game.clone()).collect()
    }

    /// Copy stored playtime onto the scanned games, matched by store and id.
    ///
    /// Rows the scan did not produce are ignored: the library is the source of
    /// truth for what is installed, the database only remembers user data.
    pub fn apply_stored(&mut self, stored: &[Game]) {
        for entry in &mut self.games {
            let Some(row) = stored.iter().find(|row| row.id == entry.game.id) else {
                continue;
            };
            entry.game.playtime = row.playtime;
            entry.game.last_played = row.last_played;
        }
    }

    /// Copy cached store metadata onto the scanned games, matched by id.
    ///
    /// Negative cache entries are copied too: they carry the fetch timestamp
    /// that keeps a delisted app from being retried on every launch.
    pub fn apply_metadata(&mut self, rows: &[StoredMetadata]) {
        for entry in &mut self.games {
            if let Some(row) = rows.iter().find(|row| row.game == entry.game.id) {
                entry.metadata = Some(row.clone());
            }
        }
    }

    /// Appids whose cached answer is missing or older than `policy`.
    ///
    /// Returned in library order, deduplicated; non-numeric ids are skipped
    /// (Steam appids always parse, other stores will get their own path).
    pub fn metadata_candidates(&self, policy: CachePolicy, now: OffsetDateTime) -> Vec<u32> {
        let mut ids = Vec::new();
        for entry in &self.games {
            let fresh = entry
                .metadata
                .as_ref()
                .is_some_and(|row| policy.is_fresh(row.fetched_at, row.metadata.is_some(), now));
            if fresh {
                continue;
            }
            let Ok(app_id) = entry.game.id.store_id.parse::<u32>() else {
                continue;
            };
            if !ids.contains(&app_id) {
                ids.push(app_id);
            }
        }
        ids
    }

    /// Every art image the library wants, deduplicated.
    ///
    /// Games without metadata (or with a negative cache row) contribute
    /// nothing; the art warmer skips images that are already on disk.
    pub fn art_requests(&self) -> Vec<ArtRequest> {
        let mut requests: Vec<ArtRequest> = Vec::new();
        for entry in &self.games {
            let Some(metadata) = entry.store_metadata() else {
                continue;
            };
            for request in crate::art::requests_for(metadata) {
                if !requests.contains(&request) {
                    requests.push(request);
                }
            }
        }
        requests
    }
}

/// Ask the running Steam client to start a game.
///
/// Steam then runs the game through its own launch pipeline — including the
/// `pitagames-shim <appid> %command%` options when the user has configured
/// them. Errors are caller-visible: the UI decides how to surface them.
pub fn spawn_steam(app_id: &str) -> std::io::Result<()> {
    spawn(&steam_run(app_id))
}

/// Ask Steam to install or update an app; the caller then follows the manifest.
pub fn spawn_install(app_id: &str) -> std::io::Result<()> {
    spawn(&steam_install(app_id))
}

fn spawn(spec: &pitagames_core::LaunchSpec) -> std::io::Result<()> {
    let child = std::process::Command::new(&spec.program)
        .args(&spec.args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let _ = child.id(); // `steam` hands off to its daemon; we only needed the ask.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{GameId, Store};

    fn entry(app_id: &str, title: &str, phase: InstallPhase, installed: bool) -> GameEntry {
        let mut game = Game::new(GameId::new(Store::Steam, app_id), title);
        game.installed = installed;
        GameEntry {
            game,
            phase,
            metadata: None,
        }
    }

    fn library() -> Library {
        let mut dota = entry("570", "Dota 2", InstallPhase::Installed, true);
        dota.game.size_bytes = Some(48_318_382_080);
        let mut portal = entry("620", "portal", InstallPhase::Installed, true);
        portal.game.playtime = Duration::from_secs(90 * 60);

        Library {
            games: vec![
                dota,
                portal,
                entry("999", "Uninstalled Demo", InstallPhase::NotInstalled, false),
            ],
            warnings: Vec::new(),
            metadata_warnings: Vec::new(),
            metadata_pending: false,
            steam_root: None,
            libraries: Vec::new(),
        }
    }

    #[test]
    fn filtering_is_case_insensitive_and_order_preserving() {
        let library = library();
        assert_eq!(library.filtered("").len(), 3);

        let titles: Vec<&str> = library
            .filtered("DOTA")
            .iter()
            .map(|entry| entry.title())
            .collect();
        assert_eq!(titles, vec!["Dota 2"]);

        let titles: Vec<&str> = library
            .filtered(" Portal ")
            .iter()
            .map(|entry| entry.title())
            .collect();
        assert_eq!(titles, vec!["portal"]);
    }

    #[test]
    fn filtering_misses_nonmatches() {
        assert!(library().filtered("half-life").is_empty());
    }

    #[test]
    fn play_and_install_eligibility_follows_the_phase() {
        assert!(entry("620", "Portal 2", InstallPhase::Installed, true).can_play());
        assert!(
            entry("620", "Portal 2", InstallPhase::UpdateAvailable, true).can_play(),
            "an update waiting does not block play"
        );
        assert!(!entry("620", "Portal 2", InstallPhase::Downloading, true).can_play());
        assert!(!entry("620", "Portal 2", InstallPhase::NotInstalled, false).can_play());

        assert!(entry("620", "Portal 2", InstallPhase::UpdateAvailable, true).can_install());
        assert!(entry("620", "Portal 2", InstallPhase::NotInstalled, false).can_install());
        assert!(!entry("620", "Portal 2", InstallPhase::Installed, true).can_install());
        assert!(!entry("620", "Portal 2", InstallPhase::Downloading, true).can_install());
    }

    #[test]
    fn status_labels_skip_a_plain_install() {
        assert_eq!(
            entry("620", "Portal 2", InstallPhase::Installed, true).status_label(),
            None
        );
        assert_eq!(
            entry("620", "Portal 2", InstallPhase::UpdateAvailable, true).status_label(),
            Some("update available")
        );
        assert_eq!(
            entry("620", "Portal 2", InstallPhase::Downloading, true).status_label(),
            Some("downloading")
        );
        assert_eq!(
            entry("620", "Portal 2", InstallPhase::Unknown, true).status_label(),
            Some("state unknown")
        );
    }

    #[test]
    fn in_flight_lists_only_busy_phases() {
        let mut library = library();
        library
            .games
            .push(entry("400", "Half-Life", InstallPhase::Downloading, true));
        library.games[1].phase = InstallPhase::UpdateAvailable;

        let in_flight: Vec<&str> = library
            .in_flight()
            .iter()
            .map(|entry| entry.app_id())
            .collect();
        assert_eq!(in_flight, vec!["400"]);
    }

    #[test]
    fn meta_line_shows_size_and_playtime_only_when_known() {
        let library = library();
        assert_eq!(library.games[0].meta_line(), "steam · appid 570 · 45.0 GiB");
        assert_eq!(
            library.games[1].meta_line(),
            "steam · appid 620 · played 1.5 h"
        );
        assert_eq!(library.games[2].meta_line(), "steam · appid 999");
    }

    #[test]
    fn absorb_keeps_one_row_per_appid_and_prefers_installed() {
        let mut base = Library::default();
        base.absorb(Library {
            games: vec![entry("620", "Portal 2", InstallPhase::NotInstalled, false)],
            warnings: vec!["w1".into()],
            metadata_warnings: vec!["m1".into()],
            metadata_pending: false,
            steam_root: Some(PathBuf::from("/steam")),
            libraries: vec![PathBuf::from("/steam")],
        });
        base.absorb(Library {
            games: vec![
                entry("620", "Portal 2", InstallPhase::Installed, true),
                entry("570", "Dota 2", InstallPhase::Installed, true),
            ],
            warnings: vec!["w2".into()],
            metadata_warnings: vec!["m2".into()],
            metadata_pending: true,
            steam_root: Some(PathBuf::from("/steam2")),
            libraries: vec![PathBuf::from("/steam"), PathBuf::from("/other")],
        });

        assert_eq!(base.games.len(), 2);
        assert!(base.games[0].game.installed, "installed copy wins");
        assert_eq!(base.warnings, ["w1", "w2"]);
        assert_eq!(base.metadata_warnings, ["m1", "m2"]);
        assert_eq!(base.steam_root.as_deref(), Some(Path::new("/steam")));
        assert_eq!(
            base.libraries,
            [PathBuf::from("/steam"), PathBuf::from("/other")]
        );
    }

    #[test]
    fn stored_playtime_lands_on_the_scanned_rows() {
        let mut library = library();
        let mut stored_row = Game::new(GameId::new(Store::Steam, "570"), "Dota 2");
        stored_row.playtime = Duration::from_secs(12 * 3600);
        stored_row.last_played = Some(time::OffsetDateTime::UNIX_EPOCH);

        library.apply_stored(&[stored_row]);

        assert_eq!(library.games[0].playtime(), Duration::from_secs(12 * 3600));
        assert!(library.games[0].game.last_played.is_some());
        assert_eq!(
            library.games[1].playtime(),
            Duration::from_secs(90 * 60),
            "rows the database does not know keep the scan value"
        );
    }

    fn stored_metadata(app_id: &str, fetched_at: OffsetDateTime, found: bool) -> StoredMetadata {
        let game = GameId::new(Store::Steam, app_id);
        let metadata = found.then(|| {
            let mut metadata =
                pitagames_core::GameMetadata::new(game.clone(), format!("Game {app_id}"));
            metadata.genres = vec!["Action".into()];
            metadata
        });
        StoredMetadata {
            game,
            metadata,
            fetched_at,
        }
    }

    #[test]
    fn cached_metadata_lands_on_the_scanned_rows() {
        let mut library = library();
        let fetched_at = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();

        library.apply_metadata(&[
            stored_metadata("620", fetched_at, true),
            stored_metadata("570", fetched_at, false),
        ]);

        let portal = &library.games[1];
        assert_eq!(
            portal
                .store_metadata()
                .map(|metadata| metadata.genres.clone()),
            Some(vec!["Action".to_string()])
        );
        assert_eq!(portal.metadata_fetched_at(), Some(fetched_at));

        assert!(
            library.games[0].store_metadata().is_none(),
            "a negative cache entry shows nothing"
        );
        assert!(library.games[0].metadata.is_some());
        assert!(
            library.games[2].metadata.is_none(),
            "unknown ids are left alone"
        );
    }

    #[test]
    fn every_game_without_cached_metadata_is_a_candidate() {
        let library = library();
        assert_eq!(
            library.metadata_candidates(CachePolicy::DEFAULT, OffsetDateTime::UNIX_EPOCH),
            vec![570, 620, 999]
        );
    }

    #[test]
    fn metadata_candidates_skip_fresh_rows_and_retry_stale_ones() {
        let mut library = library();
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();

        library.games[0].metadata = Some(stored_metadata("570", now, true));
        library.games[1].metadata =
            Some(stored_metadata("620", now - time::Duration::days(8), true));
        library.games[2].metadata = Some(stored_metadata(
            "999",
            now - time::Duration::hours(2),
            false,
        ));

        assert_eq!(
            library.metadata_candidates(CachePolicy::DEFAULT, now),
            vec![620],
            "fresh positive and fresh negative rows are reused"
        );

        library.games[2].metadata = Some(stored_metadata(
            "999",
            now - time::Duration::hours(25),
            false,
        ));
        assert_eq!(
            library.metadata_candidates(CachePolicy::DEFAULT, now),
            vec![620, 999],
            "an expired negative answer is retried"
        );
    }

    #[test]
    fn art_requests_cover_capsule_and_hero_and_skip_games_without_metadata() {
        let mut library = library();
        let mut metadata =
            pitagames_core::GameMetadata::new(GameId::new(Store::Steam, "620"), "Portal 2");
        metadata.capsule_image = Some("https://cdn/capsule.jpg".into());
        metadata.header_image = Some("https://cdn/header.jpg".into());
        library.games[1].metadata = Some(StoredMetadata {
            game: metadata.game.clone(),
            metadata: Some(metadata),
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        });

        let requests = library.art_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0],
            ArtRequest::new("https://cdn/capsule.jpg", pitagames_art::ArtSize::Grid)
        );
        assert_eq!(
            requests[1],
            ArtRequest::new("https://cdn/header.jpg", pitagames_art::ArtSize::Hero)
        );

        // The other games have no cached metadata: no requests, no placeholders
        // pretending an image is coming.
        assert!(
            requests.iter().all(|request| request.url.contains("cdn")),
            "only the game with metadata contributes art"
        );
    }
}
