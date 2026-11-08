//! The pre-compiled per-game profile that `pitagames-shim` reads at launch.
//!
//! Steam's launch options route a game through the shim:
//!
//! ```text
//! pitagames-shim <appid> %command%
//! ```
//!
//! Steam only substitutes the game's command line — it knows nothing about
//! profiles — so everything the launcher wants to apply lives in a small JSON
//! file next to the database:
//!
//! ```text
//! ~/.local/share/pitagames/profiles/steam-<appid>.json
//! ```
//!
//! The GUI compiles a [`Profile`](crate::Profile) into this file on every save
//! (see `platform::envc`); the shim reads it, applies the environment and
//! wrappers, and runs the game. Writing it is the only file I/O in this crate
//! besides path resolution — it is the launcher's own file, not a store's, so it
//! stays here where the GUI, the CLI and the shim all share one contract.
//!
//! The file is written atomically (temp file, `fsync`, `rename`) exactly like
//! `localconfig.vdf`: a half-written profile must never reach a launch. A
//! missing or corrupt file is not fatal — the shim falls back to running the
//! game unchanged and logs a warning.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::profile::{ProtonRef, WrapperStep};
use crate::store::GameId;

/// Schema version of the on-disk profile. Bumped when the shape changes in a
/// way older shims cannot read.
pub const SHIM_PROFILE_SCHEMA: u32 = 1;

/// Everything the shim needs to launch one game.
///
/// `env` and `wrappers` are ordered and may contain duplicates: later entries
/// win, matching [`LaunchSpec`](crate::LaunchSpec) semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimProfile {
    /// Format version; see [`SHIM_PROFILE_SCHEMA`].
    pub schema: u32,
    pub game: GameId,
    /// The named profile this file was compiled from, for the transparency UI.
    pub profile: String,
    /// The Proton choice, recorded (and resolved when known) for display. The
    /// shim does not apply it: for a Steam game, Steam picks the runtime.
    #[serde(default)]
    pub proton: Option<ShimProton>,
    /// Environment additions, in application order.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Ordered process wrappers, outermost first.
    #[serde(default)]
    pub wrappers: Vec<WrapperStep>,
    /// Where the shim writes its own log line, if anywhere.
    #[serde(default)]
    pub log_path: Option<PathBuf>,
    /// When the file was written, for the transparency UI.
    #[serde(default)]
    pub written_at: Option<OffsetDateTime>,
}

impl ShimProfile {
    /// A fresh profile with no environment and no wrappers.
    pub fn new(game: GameId, profile: impl Into<String>) -> Self {
        Self {
            schema: SHIM_PROFILE_SCHEMA,
            game,
            profile: profile.into(),
            proton: None,
            env: Vec::new(),
            wrappers: Vec::new(),
            log_path: None,
            written_at: Some(OffsetDateTime::now_utc()),
        }
    }
}

/// The Proton choice recorded in a [`ShimProfile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimProton {
    /// What the profile asked for ([`ProtonRef::Auto`] by default).
    pub reference: ProtonRef,
    /// The build discovery resolved, when the reference does not let Steam
    /// decide. `None` for [`ProtonRef::SteamDefault`] and for an unresolved
    /// [`ProtonRef::Auto`].
    #[serde(default)]
    pub install: Option<ShimProtonInstall>,
}

/// A resolved Proton build, flattened to strings so this crate stays
/// independent of `pitagames-platform`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimProtonInstall {
    pub name: String,
    pub path: PathBuf,
    /// Human label of the family (`proton-cachyos`, `GE-Proton`, …).
    pub kind: String,
    #[serde(default)]
    pub version: Option<String>,
    /// Human label of where it was found (`system`, `user`, `steam`, `umu`).
    pub source: String,
}

/// `<dir>/<store>-<store_id>.json`, the shim's profile path for a game.
pub fn shim_profile_path(dir: &Path, id: &GameId) -> PathBuf {
    dir.join(format!("{}-{}.json", id.store.as_str(), id.store_id))
}

/// Read the shim profile for a game, or `None` when it has never been written.
pub fn load_shim_profile(dir: &Path, id: &GameId) -> Result<Option<ShimProfile>, ProfileFileError> {
    let path = shim_profile_path(dir, id);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(decode(&path, &bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProfileFileError::Io {
            action: "read",
            path,
            source,
        }),
    }
}

/// Read a shim profile from an explicit path (the CLI and the shim).
pub fn read_shim_profile(path: &Path) -> Result<ShimProfile, ProfileFileError> {
    let bytes = fs::read(path).map_err(|source| ProfileFileError::Io {
        action: "read",
        path: path.to_path_buf(),
        source,
    })?;
    decode(path, &bytes)
}

fn decode(path: &Path, bytes: &[u8]) -> Result<ShimProfile, ProfileFileError> {
    let profile: ShimProfile =
        serde_json::from_slice(bytes).map_err(|source| ProfileFileError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    if profile.schema != SHIM_PROFILE_SCHEMA {
        return Err(ProfileFileError::Schema {
            path: path.to_path_buf(),
            found: profile.schema,
            expected: SHIM_PROFILE_SCHEMA,
        });
    }
    Ok(profile)
}

/// Write a shim profile atomically, returning the path written.
pub fn save_shim_profile(dir: &Path, profile: &ShimProfile) -> Result<PathBuf, ProfileFileError> {
    let path = shim_profile_path(dir, &profile.game);
    let mut bytes =
        serde_json::to_vec_pretty(profile).map_err(|source| ProfileFileError::Json {
            path: path.clone(),
            source,
        })?;
    bytes.push(b'\n');
    atomic_write(&path, &bytes)?;
    Ok(path)
}

/// Remove a game's shim profile. `Ok(false)` means it was already gone.
pub fn remove_shim_profile(dir: &Path, id: &GameId) -> Result<bool, ProfileFileError> {
    let path = shim_profile_path(dir, id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ProfileFileError::Io {
            action: "remove",
            path,
            source,
        }),
    }
}

/// Write `bytes` through a sibling temp file so a reader never sees half a file.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProfileFileError> {
    let directory = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&directory).map_err(|source| ProfileFileError::Io {
        action: "create parent of",
        path: path.to_path_buf(),
        source,
    })?;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "profile".to_string());
    let stamp = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let temp = directory.join(format!(".{name}.tmp-{}-{stamp}", process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        // Best-effort durability of the rename itself.
        if let Ok(dir) = fs::File::open(&directory) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if let Err(source) = result {
        let _ = fs::remove_file(&temp);
        return Err(ProfileFileError::Io {
            action: "write",
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

/// Shim-profile file failures.
#[derive(Debug, thiserror::Error)]
pub enum ProfileFileError {
    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("shim profile {path} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("shim profile {path} uses schema {found}, this build writes {expected}")]
    Schema {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn game() -> GameId {
        GameId::new(Store::Steam, "620")
    }

    #[test]
    fn the_path_follows_the_store_and_id() {
        let dir = Path::new("/data/pitagames/profiles");
        assert_eq!(
            shim_profile_path(dir, &game()),
            PathBuf::from("/data/pitagames/profiles/steam-620.json")
        );
        assert_eq!(
            shim_profile_path(dir, &GameId::new(Store::Epic, "Portal")),
            PathBuf::from("/data/pitagames/profiles/epic-Portal.json")
        );
    }

    #[test]
    fn a_saved_profile_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let mut profile = ShimProfile::new(game(), "FSR 4");
        profile.env.push(("DXVK_HUD".into(), "fps".into()));
        profile.env.push(("PROTON_LOG".into(), "1".into()));
        profile.wrappers.push(WrapperStep {
            program: PathBuf::from("/usr/bin/gamemoderun"),
            args: Vec::new(),
        });
        profile.proton = Some(ShimProton {
            reference: ProtonRef::CachyOs,
            install: Some(ShimProtonInstall {
                name: "proton-cachyos".into(),
                path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
                kind: "proton-cachyos".into(),
                version: Some("10.0-20251101".into()),
                source: "system".into(),
            }),
        });

        let path = save_shim_profile(temp.path(), &profile).unwrap();
        assert_eq!(path, temp.path().join("steam-620.json"));

        let loaded = load_shim_profile(temp.path(), &game()).unwrap().unwrap();
        assert_eq!(loaded, profile);

        // The JSON shape is part of the on-disk contract.
        let json = fs::read_to_string(&path).unwrap();
        assert!(json.contains(r#""schema": 1"#), "{json}");
        assert!(json.contains(r#""kind": "cachy_os""#), "{json}");
        assert!(
            json.contains(r#""program": "/usr/bin/gamemoderun""#),
            "{json}"
        );
    }

    #[test]
    fn a_missing_profile_is_none_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load_shim_profile(temp.path(), &game()).unwrap().is_none());
    }

    #[test]
    fn removing_reports_whether_the_file_existed() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!remove_shim_profile(temp.path(), &game()).unwrap());
        save_shim_profile(temp.path(), &ShimProfile::new(game(), "Default")).unwrap();
        assert!(remove_shim_profile(temp.path(), &game()).unwrap());
        assert!(load_shim_profile(temp.path(), &game()).unwrap().is_none());
    }

    #[test]
    fn corrupt_json_is_reported_with_the_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = shim_profile_path(temp.path(), &game());
        fs::write(&path, "{not json").unwrap();

        let error = load_shim_profile(temp.path(), &game()).unwrap_err();
        assert!(matches!(error, ProfileFileError::Json { .. }), "{error}");
        assert!(error.to_string().contains("steam-620.json"), "{error}");
    }

    #[test]
    fn an_unknown_schema_is_refused_rather_than_guessed() {
        let temp = tempfile::tempdir().unwrap();
        let path = shim_profile_path(temp.path(), &game());
        let mut profile = ShimProfile::new(game(), "Default");
        profile.schema = 99;
        fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();

        let error = load_shim_profile(temp.path(), &game()).unwrap_err();
        assert!(
            matches!(error, ProfileFileError::Schema { found: 99, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_write_leaves_no_temp_files_behind() {
        let temp = tempfile::tempdir().unwrap();
        save_shim_profile(temp.path(), &ShimProfile::new(game(), "Default")).unwrap();

        let names: Vec<String> = fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["steam-620.json"]);
    }

    #[test]
    fn writing_creates_missing_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("a/b/c");
        let path = save_shim_profile(&nested, &ShimProfile::new(game(), "Default")).unwrap();
        assert!(path.is_file());
    }
}
