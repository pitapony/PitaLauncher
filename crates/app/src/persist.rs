//! SQLite persistence for the GUI.
//!
//! The database is a cache of scan results plus the user's own playtime; a
//! missing or broken file must never stop the launcher, so callers get a
//! display-ready error string and carry on with an unsaved library.

use std::time::Duration;

use pitagames_core::{Game, GameId, Profile};
use pitagames_db::{
    Db, GameRepository, MetadataRepository, ProfileRepository, StoredMetadata, StoredProfile,
};
use time::OffsetDateTime;

/// Open (or create) `<XDG_DATA_HOME>/pitagames/pitagames.db`.
pub async fn open_default() -> Result<Db, String> {
    let dir = pitagames_core::paths::data_dir()
        .ok_or_else(|| "no data directory (HOME is unset)".to_string())?;
    let path = dir.join("pitagames.db");

    Db::open(&path)
        .await
        .map_err(|error| format!("could not open {}: {error}", path.display()))
}

/// Upsert a scan and return the stored rows, which carry saved playtime.
///
/// The upsert deliberately leaves `playtime_secs` and `last_played` alone (see
/// the `ON CONFLICT` clause in the migration): those are user data, a scan only
/// refreshes what Steam knows. Steam's own playtime, read from
/// `localconfig.vdf`, is pushed through `set_playtime` instead.
pub async fn sync(db: &Db, scanned: &[Game]) -> Result<Vec<Game>, String> {
    db.upsert_games(scanned)
        .await
        .map_err(|error| format!("could not save the library: {error}"))?;

    for game in scanned.iter().filter(|game| !game.playtime.is_zero()) {
        db.set_playtime(&game.id, game.playtime, game.last_played)
            .await
            .map_err(|error| {
                format!("could not save playtime for {}: {error}", game.id.store_id)
            })?;
    }

    db.games()
        .await
        .map_err(|error| format!("could not read the library back: {error}"))
}

/// Record a launch: `last_played` moves, playtime stays what it was.
pub async fn record_launch(db: &Db, id: &GameId, playtime: Duration) -> Result<(), String> {
    db.set_playtime(id, playtime, Some(OffsetDateTime::now_utc()))
        .await
        .map(|_| ())
        .map_err(|error| format!("could not record the launch of {}: {error}", id.store_id))
}

/// Profiles belonging to one game, ordered case-insensitively by name.
pub async fn profiles(db: &Db, id: &GameId) -> Result<Vec<StoredProfile>, String> {
    db.profiles(id)
        .await
        .map_err(|error| format!("could not read profiles for {}: {error}", id.store_id))
}

/// Create or update a profile, returning its row id.
///
/// An empty name is rejected here so the detail page can show the message:
/// [`Profile::validate`] runs again inside the database.
pub async fn save_profile(db: &Db, profile: &Profile) -> Result<i64, String> {
    profile
        .validate()
        .map_err(|error| format!("this profile cannot be saved: {error}"))?;
    db.save_profile(profile)
        .await
        .map_err(|error| format!("could not save the profile: {error}"))
}

/// Delete one profile; `Ok(false)` means it was already gone.
pub async fn delete_profile(db: &Db, id: &GameId, name: &str) -> Result<bool, String> {
    db.delete_profile(id, name)
        .await
        .map_err(|error| format!("could not delete profile {name:?}: {error}"))
}

/// Every cached store metadata row, including negative entries.
pub async fn load_metadata(db: &Db) -> Result<Vec<StoredMetadata>, String> {
    db.metadata_rows()
        .await
        .map_err(|error| format!("could not read cached store metadata: {error}"))
}

/// Store a batch of fetched metadata answers (positive or negative).
pub async fn save_metadata(db: &Db, rows: &[StoredMetadata]) -> Result<usize, String> {
    db.save_metadata_many(rows)
        .await
        .map_err(|error| format!("could not save store metadata: {error}"))
}
