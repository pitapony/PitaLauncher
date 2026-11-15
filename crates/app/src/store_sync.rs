//! Syncing the CLI stores' libraries into the cache the grid reads.
//!
//! The Stores screen's "Sync libraries" action is two halves with very
//! different costs. Running `legendary`, `nile` and `lgogdownloader` blocks on
//! each client for seconds (and may open a browser on a token refresh), so
//! [`fetch`] runs on a worker thread; merging the answer over the cache and
//! writing it is SQLite work, so [`apply`] is async and stays on the UI's
//! runtime. The split is also what makes both testable: a fake CLI feeds
//! `fetch`, a `tempfile` database feeds `apply`.
//!
//! The merge rules live in `pitagames_stores::sync` and are not repeated here:
//! a failed listing never deletes rows, an unanswered install listing keeps a
//! cached install, and an installed row no listing mentions survives.

use pitagames_core::{Game, Store};
use pitagames_db::{Db, GameRepository};
use pitagames_stores::cli::CliOptions;
use pitagames_stores::sync::{self, Backends, StoreLibrary, SyncReport};

/// What one store's sync did, for the Stores screen's status line and the grid.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncedStore {
    pub store: Store,
    /// Every row the store now has in the cache.
    pub rows: Vec<Game>,
    pub report: SyncReport,
}

/// Run each store's listings. Blocking: call this on a worker thread.
pub fn fetch(
    backends: &Backends,
    options: &CliOptions,
    stores: &[Store],
) -> Vec<(Store, StoreLibrary)> {
    stores
        .iter()
        .map(|store| (*store, backends.library(*store, options)))
        .collect()
}

/// Merge one store's fresh answer over the cache and write it.
///
/// An incomplete answer is not written ([`StoreLibrary::can_replace`]), which is
/// how a signed-out or missing CLI leaves the cache alone.
pub async fn apply(db: &Db, library: &StoreLibrary) -> Result<SyncedStore, String> {
    let cached = db
        .store_games(library.store)
        .await
        .map_err(|error| format!("could not read the {} rows: {error}", library.store))?;
    let rows = sync::merge(&cached, library);

    let mut removed = 0;
    let applied = library.can_replace();
    if applied {
        removed = db
            .replace_store_games(library.store, &rows)
            .await
            .map_err(|error| format!("could not save the {} rows: {error}", library.store))?
            .removed;
    }

    Ok(SyncedStore {
        store: library.store,
        report: SyncReport::new(library, &rows, removed, applied),
        rows,
    })
}

/// Apply every fetched answer, one store at a time.
///
/// A failure is recorded against its store and the rest still run: a broken
/// GOG listing must not cost the Epic rows. Returns what was written and the
/// messages the screen shows.
pub async fn apply_all(
    db: &Db,
    fetched: &[(Store, StoreLibrary)],
) -> (Vec<SyncedStore>, Vec<String>) {
    let mut synced = Vec::new();
    let mut problems = Vec::new();

    for (_, library) in fetched {
        match apply(db, library).await {
            Ok(store) => {
                let mut messages: Vec<String> = store.report.warnings.clone();
                problems.append(&mut messages);
                synced.push(store);
            }
            Err(message) => problems.push(message),
        }
    }

    (synced, problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_stores::amazon::Amazon;
    use pitagames_stores::epic::Epic;
    use pitagames_stores::gog::Gog;
    use std::fs;
    use std::path::{Path, PathBuf};

    const LEGENDARY_LIST: &str = include_str!("../../../fixtures/legendary/list.json");
    const LEGENDARY_INSTALLED: &str =
        include_str!("../../../fixtures/legendary/list-installed.json");

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

    /// A `legendary` that answers both epic listings from the fixtures.
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

    fn backends(epic: &str) -> Backends {
        Backends {
            epic: Epic::with_program(epic),
            amazon: Amazon::with_program("/nonexistent/nile"),
            gog: Gog::with_programs("/nonexistent/lgogdownloader", "/nonexistent/gogdl"),
        }
    }

    #[test]
    fn fetch_returns_one_answer_per_requested_store() {
        let dir = tempfile::tempdir().unwrap();
        let legendary = legendary(dir.path());
        let backends = backends(legendary.to_str().unwrap());

        let fetched = fetch(&backends, &options(), &[Store::Epic, Store::Amazon]);
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].0, Store::Epic);
        assert!(fetched[0].1.can_replace(), "{:?}", fetched[0].1.warnings);
        assert_eq!(fetched[1].0, Store::Amazon);
        assert!(!fetched[1].1.can_replace(), "nile is not installed");
    }

    #[tokio::test]
    async fn apply_writes_the_rows_into_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let legendary = legendary(dir.path());
        let backends = backends(legendary.to_str().unwrap());
        let db = Db::open_in_memory().await.unwrap();

        let fetched = fetch(&backends, &options(), &[Store::Epic]);
        let (synced, problems) = apply_all(&db, &fetched).await;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(synced.len(), 1);
        assert_eq!(synced[0].store, Store::Epic);
        assert!(synced[0].report.applied);
        assert_eq!(synced[0].report.rows, 2, "an installed and an owned row");

        let cached = db.store_games(Store::Epic).await.unwrap();
        assert_eq!(cached.len(), 2);
        assert!(cached.iter().any(|game| game.installed));
        assert!(synced[0].rows.iter().all(|row| row.id.store == Store::Epic));
    }

    #[tokio::test]
    async fn a_failed_listing_leaves_the_cache_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().await.unwrap();

        // Seed the cache through a working sync.
        let good = legendary(dir.path());
        let fetched = fetch(
            &backends(good.to_str().unwrap()),
            &options(),
            &[Store::Epic],
        );
        let _ = apply_all(&db, &fetched).await;
        let before = db.store_games(Store::Epic).await.unwrap();
        assert!(!before.is_empty());

        // Now the CLI is signed out: both listings fail.
        let broken = script(
            dir.path(),
            "legendary-broken",
            "#!/bin/sh\necho '[cli] ERROR: No saved credentials' >&2\nexit 0\n",
        );
        let fetched = fetch(
            &backends(broken.to_str().unwrap()),
            &options(),
            &[Store::Epic],
        );
        let (synced, problems) = apply_all(&db, &fetched).await;

        assert_eq!(synced.len(), 1);
        assert!(
            !synced[0].report.applied,
            "an incomplete answer is not written"
        );
        assert_eq!(synced[0].report.removed, 0);
        assert!(!problems.is_empty(), "the failure is reported");
        assert_eq!(
            db.store_games(Store::Epic).await.unwrap(),
            before,
            "a failed listing never deletes rows"
        );
    }

    #[tokio::test]
    async fn one_broken_store_does_not_stop_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().await.unwrap();
        let legendary = legendary(dir.path());
        let backends = backends(legendary.to_str().unwrap());

        let fetched = fetch(&backends, &options(), &[Store::Gog, Store::Epic]);
        let (synced, problems) = apply_all(&db, &fetched).await;

        assert_eq!(synced.len(), 2);
        assert!(!synced[0].report.applied, "gog has no CLI here");
        assert!(synced[1].report.applied);
        assert!(
            problems.iter().any(|problem| problem.contains("gog")),
            "{problems:?}"
        );
    }
}
