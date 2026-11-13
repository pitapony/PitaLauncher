//! Per-game Wine/Proton prefixes for the non-Steam stores.
//!
//! Steam owns its prefixes (`steamapps/compatdata/<appid>`), so it never asks
//! this module for anything. Every other store — Epic, Amazon, GOG, an imported
//! Heroic/Lutris/Bottles entry — gets one prefix *per game* under
//! `<XDG_DATA_HOME>/pitagames/prefixes/<store>-<id>`, because a shared prefix is
//! how two games end up overwriting each other's registry, saves and installed
//! runtimes.
//!
//! This module owns the layout only: where a game's prefix goes, how its store
//! id becomes a directory name, whether Proton has already created the prefix,
//! how much room it takes, and the environment `umu-run` needs to point at it.
//! It never creates, deletes or launches a prefix — the umu/Proton hand-off and
//! the "reclaim disk space" action are later items, and both need the paths this
//! reports. Read-only and framework-free, like [`crate::proton`].
//!
//! Prefixes live outside the game directory on purpose: a store CLI can
//! re-download or repair the game tree without touching the prefix, and a
//! "verify install" cannot silently reset a game's settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pitagames_core::paths;
use pitagames_core::store::GameId;
use serde::Serialize;

use crate::disk::{self, DirSize};

/// Replaces the default prefix root.
///
/// Set and non-empty, it is where every non-Steam prefix goes, so a user whose
/// games live on a second disk can keep the prefixes beside them.
pub const PREFIX_DIR_ENV: &str = "PITAGAMES_PREFIX_DIR";

/// Files that mean Proton has created a prefix in a directory.
///
/// `drive_c` is the surest, but a prefix interrupted during creation still has
/// the registry files, so any one of them counts as "started".
pub const INIT_MARKERS: &[&str] = &["drive_c", "system.reg", "user.reg"];

/// The default prefix root: `$PITAGAMES_PREFIX_DIR`, else
/// `<XDG_DATA_HOME>/pitagames/prefixes`.
#[must_use]
pub fn default_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(PREFIX_DIR_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    paths::prefixes_dir()
}

/// The umu/app key for one game: `<store>-<id>`, sanitized to one component.
///
/// `GameId`s come from store JSON and command lines, so the combined string is
/// run through the same [`paths::safe_component`] the install directories use:
/// no separator, no control character, and never `.`/`..`. This is also the
/// `GAMEID` umu sees, so it must stay stable across launches.
#[must_use]
pub fn game_key(game: &GameId) -> String {
    let raw = format!("{}-{}", game.store.as_str(), game.store_id);
    paths::safe_component(&raw, "game")
}

/// `<root>/<store>-<id>`: one game's prefix under an explicit root.
#[must_use]
pub fn dir_in(root: &Path, game: &GameId) -> PathBuf {
    root.join(game_key(game))
}

/// The default prefix directory for one game.
#[must_use]
pub fn dir(game: &GameId) -> Option<PathBuf> {
    default_root().map(|root| dir_in(&root, game))
}

/// Whether Proton has already created a prefix in `path`.
#[must_use]
pub fn is_initialized(path: &Path) -> bool {
    INIT_MARKERS.iter().any(|marker| path.join(marker).exists())
}

/// The environment `umu-run` needs to use one game's prefix and Proton build.
///
/// `GAMEID` is what umu-protonfixes matches on, `PROTONPATH` picks the build in
/// the sane order the plan requires, and `WINEPREFIX` forces the per-game
/// prefix instead of Wine's default `~/.wine`. The compiler merges these into
/// the `LaunchSpec`; this module only supplies the three values it owns.
#[must_use]
pub fn umu_env(game: &GameId, proton: &Path, prefix: &Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("GAMEID".to_string(), game_key(game)),
        ("PROTONPATH".to_string(), proton.display().to_string()),
        ("WINEPREFIX".to_string(), prefix.display().to_string()),
    ])
}

/// The state of one game's prefix, as the GUI and the CLI render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Prefix {
    /// The root the path was derived from.
    pub root: PathBuf,
    /// The umu/app key (`<store>-<id>`).
    pub key: String,
    /// `<root>/<key>`.
    pub path: PathBuf,
    /// The directory exists.
    pub present: bool,
    /// Proton has created the prefix inside it.
    pub initialized: bool,
    /// Bytes under the directory (bounded; see [`disk::dir_size`]).
    pub size: DirSize,
}

impl Prefix {
    /// The directory that exists and holds a usable prefix.
    pub fn is_ready(&self) -> bool {
        self.present && self.initialized
    }
}

/// Inspect one game's prefix under an explicit root.
///
/// Read-only: the directory is stat-ed and (if present) walked for its size,
/// never created. `inspect` is what the CLI prints and what a "reclaim space"
/// prompt would size up before offering a delete.
#[must_use]
pub fn inspect(root: &Path, game: &GameId) -> Prefix {
    let path = dir_in(root, game);
    let present = path.is_dir();
    Prefix {
        key: game_key(game),
        present,
        initialized: present && is_initialized(&path),
        size: if present {
            disk::dir_size(&path)
        } else {
            DirSize::default()
        },
        path,
        root: root.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::Store;
    use std::fs;
    use std::sync::Mutex;

    /// Tests that read or write [`PREFIX_DIR_ENV`] must hold this lock.
    ///
    /// No other module reads this variable, so the lock is local (unlike the
    /// store CLIs' shared `TEST_ENV_LOCK`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn game(store: Store, id: &str) -> GameId {
        GameId::new(store, id)
    }

    #[test]
    fn the_key_combines_store_and_id() {
        assert_eq!(game_key(&game(Store::Epic, "Gizmo")), "epic-Gizmo");
        assert_eq!(game_key(&game(Store::Gog, "1207660623")), "gog-1207660623");
        assert_eq!(
            game_key(&game(Store::Amazon, "amzn1.adg.product.1")),
            "amazon-amzn1.adg.product.1"
        );
    }

    #[test]
    fn a_hostile_id_cannot_escape_the_root() {
        let root = Path::new("/data/pitagames/prefixes");

        assert_eq!(
            game_key(&game(Store::Epic, "../../etc/passwd")),
            "epic-..-..-etc-passwd"
        );

        // Every hostile id still produces exactly one path component under the
        // root, never a traversal.
        for hostile in ["../../etc/passwd", "..", ".", "   ", "a/b\\c", "tab\there"] {
            let id = game(Store::Epic, hostile);
            let key = game_key(&id);
            assert!(!key.is_empty(), "{hostile}");
            assert!(!key.contains(['/', '\\']), "{key}");
            assert!(key != "." && key != "..", "{key}");
            assert_eq!(dir_in(root, &id).parent(), Some(root), "{key}");
        }
    }

    #[test]
    fn dir_in_is_root_plus_key() {
        let root = Path::new("/data/pitagames/prefixes");
        assert_eq!(
            dir_in(root, &game(Store::Epic, "Gizmo")),
            PathBuf::from("/data/pitagames/prefixes/epic-Gizmo")
        );
    }

    #[test]
    fn the_env_override_replaces_the_root() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // SAFETY: only tests that mutate PITAGAMES_PREFIX_DIR run under this lock.
        unsafe { std::env::set_var(PREFIX_DIR_ENV, "/mnt/fast/pitagames-prefixes") };
        let root = default_root();
        unsafe { std::env::remove_var(PREFIX_DIR_ENV) };
        drop(_guard);

        assert_eq!(
            root,
            Some(PathBuf::from("/mnt/fast/pitagames-prefixes")),
            "the explicit root must win over the XDG default"
        );
    }

    #[test]
    fn inspect_reports_a_missing_prefix() {
        let root = tempfile::tempdir().unwrap();
        let prefix = inspect(root.path(), &game(Store::Epic, "Gizmo"));

        assert!(!prefix.present);
        assert!(!prefix.initialized);
        assert!(!prefix.is_ready());
        assert_eq!(prefix.key, "epic-Gizmo");
        assert_eq!(prefix.path, root.path().join("epic-Gizmo"));
        assert_eq!(prefix.size.summary(), "not present");
    }

    #[test]
    fn a_present_but_uninitialized_directory_is_not_ready() {
        let root = tempfile::tempdir().unwrap();
        let path = dir_in(root.path(), &game(Store::Gog, "1234"));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("notes.txt"), b"hello").unwrap();

        let prefix = inspect(root.path(), &game(Store::Gog, "1234"));
        assert!(prefix.present);
        assert!(!prefix.initialized, "no drive_c or registry files yet");
        assert!(!prefix.is_ready());
        assert_eq!(prefix.size.bytes, 5);
    }

    #[test]
    fn a_prefix_with_drive_c_is_initialized() {
        let root = tempfile::tempdir().unwrap();
        let path = dir_in(root.path(), &game(Store::Amazon, "amzn1.1"));
        fs::create_dir_all(path.join("drive_c/windows")).unwrap();

        let prefix = inspect(root.path(), &game(Store::Amazon, "amzn1.1"));
        assert!(prefix.initialized);
        assert!(prefix.is_ready());
        assert_eq!(prefix.size.dirs, 2);
    }

    #[test]
    fn a_registry_file_alone_counts_as_started() {
        let root = tempfile::tempdir().unwrap();
        let path = dir_in(root.path(), &game(Store::Epic, "Gizmo"));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("user.reg"), b"WINE REGISTRY").unwrap();

        assert!(is_initialized(&path));
    }

    #[test]
    fn umu_env_names_all_three_values() {
        let env = umu_env(
            &game(Store::Epic, "Gizmo"),
            Path::new("/opt/proton-cachyos/proton"),
            Path::new("/data/pitagames/prefixes/epic-Gizmo"),
        );

        assert_eq!(env.get("GAMEID").unwrap(), "epic-Gizmo");
        assert_eq!(env.get("PROTONPATH").unwrap(), "/opt/proton-cachyos/proton");
        assert_eq!(
            env.get("WINEPREFIX").unwrap(),
            "/data/pitagames/prefixes/epic-Gizmo"
        );
        assert_eq!(env.len(), 3);
    }
}
