//! Native Steam library scanning.
//!
//! Three files matter:
//!
//! - `<steam>/steamapps/libraryfolders.vdf` lists every library folder (the
//!   multi-disk case). Both the modern object format and the pre-2021
//!   `"1" "/path"` format are accepted.
//! - `<library>/steamapps/appmanifest_<appid>.acf` describes one installed app.
//! - `<steam>/userdata/<id>/config/localconfig.vdf` carries the playtime and
//!   last-played timestamp for that Steam user's apps.
//!
//! A broken manifest or library entry must never abort the scan: it is recorded
//! as a warning and the rest of the library still loads.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pitagames_core::{Game, GameId, Store};
use time::OffsetDateTime;

use crate::acf::{AppManifest, InstallPhase};
use crate::vdf::{self, Value};

/// Steam runtime/tool apps that must not appear as games in the library.
///
/// `is_tool` also catches Proton and Steam Linux Runtime by name, because new
/// runtime appids are added regularly.
const RUNTIME_APP_IDS: &[u32] = &[
    228980,  // Steamworks Common Redistributables
    1070560, // Steam Linux Runtime 1.0 (scout)
    1391110, // Steam Linux Runtime 2.0 (soldier)
    1628350, // Steam Linux Runtime 3.0 (sniper)
    1493710, // Proton Experimental
    2180100, // Proton Hotfix
];

/// Result of one scan. Warnings are user-facing strings, already including the
/// file path, and are printed by the CLI/UI.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub games: Vec<Game>,
    /// Install phase per appid, from the winning manifest for that appid.
    ///
    /// Separate from [`Game`] because `StateFlags` is a Steam detail; the UI
    /// uses it to show "update available" / "downloading" without touching VDF.
    pub phases: HashMap<String, InstallPhase>,
    /// Every library folder the scan read, in discovery order.
    pub libraries: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// The library folders found under one Steam root.
#[derive(Debug, Default)]
pub struct LibraryList {
    pub libraries: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Resolve every library reachable from `steam_root`.
///
/// `libraryfolders.vdf` is authoritative when present; a root without one is
/// treated as a single-library install. The root itself is not added when the
/// file lists other folders, matching Steam's own behaviour.
pub fn discover_libraries(steam_root: &Path) -> LibraryList {
    let mut list = LibraryList::default();
    let steamapps = steam_root.join("steamapps");

    let mut libraries =
        read_library_folders(&steamapps.join("libraryfolders.vdf"), &mut list.warnings);
    if libraries.is_empty() {
        libraries.push(steam_root.to_path_buf());
    }

    list.libraries = libraries;
    list
}

/// Scan every library reachable from `steam_root`.
pub fn scan_steam(steam_root: &Path, include_tools: bool) -> ScanReport {
    let mut report = ScanReport::default();
    let discovery = discover_libraries(steam_root);
    report.warnings = discovery.warnings;
    report.libraries = discovery.libraries.clone();

    let mut by_app_id: HashMap<String, Game> = HashMap::new();
    for library in discovery.libraries {
        scan_library(
            &library,
            include_tools,
            &mut by_app_id,
            &mut report.phases,
            &mut report.warnings,
        );
    }

    // Playtime and last-played live outside the library manifests: one
    // `localconfig.vdf` per Steam user under `userdata/<id>/config/`.
    for localconfig in localconfig_files(steam_root, &mut report.warnings) {
        apply_localconfig(&localconfig, &mut by_app_id, &mut report.warnings);
    }

    report.games = by_app_id.into_values().collect();
    report.games.sort_by(|left, right| {
        left.title
            .to_lowercase()
            .cmp(&right.title.to_lowercase())
            .then_with(|| left.id.store_id.cmp(&right.id.store_id))
    });

    report
}

/// Every `userdata/<id>/config/localconfig.vdf` under `steam_root`.
///
/// A missing `userdata` directory is normal (Steam has not signed in yet) and
/// is not a warning; unreadable user directories are.
fn localconfig_files(steam_root: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    user_configs_in(steam_root, warnings)
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

/// Every Steam user with a `localconfig.vdf`, as `(user_id, path)`, sorted.
///
/// The GUI needs one of these to route a game through the shim (the launch
/// options live in it); a missing `userdata` directory is an empty list.
pub fn user_configs(steam_root: &Path) -> Vec<(String, PathBuf)> {
    let mut warnings = Vec::new();
    user_configs_in(steam_root, &mut warnings)
}

fn user_configs_in(steam_root: &Path, warnings: &mut Vec<String>) -> Vec<(String, PathBuf)> {
    let userdata = steam_root.join("userdata");
    let entries = match fs::read_dir(&userdata) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(format!("failed to read {}: {error}", userdata.display()));
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "failed to read an entry in {}: {error}",
                    userdata.display()
                ));
                continue;
            }
        };

        let user = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path().join("config/localconfig.vdf");
        if path.is_file() {
            files.push((user, path));
        }
    }

    files.sort();
    files
}

/// Merge one `localconfig.vdf` into the scanned games.
///
/// Shape: `"localconfig" { "apps" { "<appid>" { "Playtime" "minutes"
/// "LastPlayed" "unix-seconds" } } }`. Playtime is whole minutes; unknown
/// appids belong to games installed elsewhere and are ignored.
fn apply_localconfig(path: &Path, games: &mut HashMap<String, Game>, warnings: &mut Vec<String>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            warnings.push(format!("failed to read {}: {error}", path.display()));
            return;
        }
    };

    let root = match vdf::parse(&text) {
        Ok(root) => root,
        Err(error) => {
            warnings.push(format!("{}: {error}", path.display()));
            return;
        }
    };

    let apps = root
        .get_object("localconfig")
        .and_then(|config| config.get_object("apps"))
        .or_else(|| root.get_object("apps"));
    let Some(apps) = apps else {
        return;
    };

    for (app_id, value) in apps.iter() {
        let Value::Object(entry) = value else {
            continue;
        };
        let Some(game) = games.get_mut(app_id) else {
            continue;
        };

        if let Some(minutes) = entry.get_str("Playtime").and_then(parse_u64) {
            game.playtime = Duration::from_secs(minutes.saturating_mul(60));
        }
        if let Some(seconds) = entry.get_str("LastPlayed").and_then(parse_i64) {
            game.last_played = OffsetDateTime::from_unix_timestamp(seconds).ok();
        }
    }
}

fn parse_u64(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

fn parse_i64(text: &str) -> Option<i64> {
    text.trim().parse().ok()
}

/// Parse `libraryfolders.vdf` contents.
///
/// Modern format:
/// ```text
/// "libraryfolders" { "0" { "path" "/games/Steam" } }
/// ```
/// Legacy format:
/// ```text
/// "LibraryFolders" { "1" "/games/Steam" }
/// ```
pub fn parse_library_folders(input: &str) -> Result<Vec<PathBuf>, LibraryFoldersError> {
    let root = vdf::parse(input)?;

    let object = root
        .get_object("libraryfolders")
        .or_else(|| {
            // Tolerate a document that is only the folders object.
            root.iter()
                .all(|(key, _)| key.parse::<u32>().is_ok())
                .then_some(&root)
        })
        .ok_or(LibraryFoldersError::MissingRoot)?;

    let mut libraries: Vec<PathBuf> = Vec::new();
    for (key, value) in object.iter() {
        let path = match value {
            Value::Object(entry) => entry.get_str("path").map(PathBuf::from),
            Value::String(path) if key.parse::<u32>().is_ok() => Some(PathBuf::from(path)),
            Value::String(_) => None,
        };

        if let Some(path) = path.filter(|path| !path.as_os_str().is_empty()) {
            if !libraries.contains(&path) {
                libraries.push(path);
            }
        }
    }

    Ok(libraries)
}

fn read_library_folders(path: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(format!("failed to read {}: {error}", path.display()));
            return Vec::new();
        }
    };

    match parse_library_folders(&text) {
        Ok(libraries) => libraries,
        Err(error) => {
            // A corrupt library list still leaves the root library usable.
            warnings.push(format!("{}: {error}", path.display()));
            Vec::new()
        }
    }
}

fn scan_library(
    library: &Path,
    include_tools: bool,
    games: &mut HashMap<String, Game>,
    phases: &mut HashMap<String, InstallPhase>,
    warnings: &mut Vec<String>,
) {
    let steamapps = library.join("steamapps");
    let entries = match fs::read_dir(&steamapps) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != ErrorKind::NotFound {
                warnings.push(format!("failed to read {}: {error}", steamapps.display()));
            }
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "failed to read an entry in {}: {error}",
                    steamapps.display()
                ));
                continue;
            }
        };

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(app_id) = app_id_from_filename(file_name) else {
            continue;
        };

        let path = entry.path();
        let manifest = match AppManifest::parse_file(&path) {
            Ok(manifest) => manifest,
            Err(error) => {
                warnings.push(error.to_string());
                continue;
            }
        };

        if manifest.app_id != app_id {
            warnings.push(format!(
                "{}: appid {} does not match the filename appid {}; using the filename",
                path.display(),
                manifest.app_id,
                app_id
            ));
        }

        if !include_tools && is_tool(app_id, &manifest.name) {
            continue;
        }

        let phase = manifest.phase();
        let mut game = Game::new(GameId::new(Store::Steam, app_id.to_string()), manifest.name);
        game.install_dir = manifest
            .install_dir
            .map(|dir| steamapps.join("common").join(dir));
        game.installed = manifest.installed;
        game.size_bytes = manifest.size_on_disk;
        game.version = manifest.build_id;
        game.last_updated = manifest.last_updated;

        match games.entry(game.id.store_id.clone()) {
            Entry::Occupied(mut slot) => {
                // The same appid can linger in a library the game was moved out
                // of. Prefer the copy whose install directory actually exists.
                if should_replace(slot.get(), &game) {
                    phases.insert(game.id.store_id.clone(), phase);
                    slot.insert(game);
                }
            }
            Entry::Vacant(slot) => {
                phases.insert(game.id.store_id.clone(), phase);
                slot.insert(game);
            }
        }
    }
}

fn should_replace(current: &Game, candidate: &Game) -> bool {
    !install_dir_exists(current) && install_dir_exists(candidate)
}

fn install_dir_exists(game: &Game) -> bool {
    game.install_dir.as_deref().is_some_and(Path::is_dir)
}

fn app_id_from_filename(file_name: &str) -> Option<u32> {
    let rest = file_name.strip_prefix("appmanifest_")?;
    let digits = rest.strip_suffix(".acf")?;
    digits.parse().ok()
}

fn is_tool(app_id: u32, name: &str) -> bool {
    RUNTIME_APP_IDS.contains(&app_id)
        || name.starts_with("Proton ")
        || name.starts_with("Steam Linux Runtime")
        || name == "Steamworks Common Redistributables"
}

/// Why `libraryfolders.vdf` could not be read.
#[derive(Debug, thiserror::Error)]
pub enum LibraryFoldersError {
    #[error(transparent)]
    Vdf(#[from] vdf::ParseError),

    #[error("missing libraryfolders object")]
    MissingRoot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_app_id_from_manifest_filename() {
        assert_eq!(app_id_from_filename("appmanifest_570.acf"), Some(570));
        assert_eq!(app_id_from_filename("appmanifest_620.ACF"), None);
        assert_eq!(app_id_from_filename("appmanifest_.acf"), None);
        assert_eq!(app_id_from_filename("libraryfolders.vdf"), None);
    }

    #[test]
    fn filters_runtime_apps() {
        assert!(is_tool(228980, "Steamworks Common Redistributables"));
        assert!(is_tool(1628350, "Steam Linux Runtime 3.0 (sniper)"));
        assert!(is_tool(999, "Proton Experimental"));
        assert!(!is_tool(570, "Dota 2"));
    }
}
