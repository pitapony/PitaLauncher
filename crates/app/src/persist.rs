//! SQLite persistence for the GUI.
//!
//! The database is a cache of scan results plus the user's own playtime; a
//! missing or broken file must never stop the launcher, so callers get a
//! display-ready error string and carry on with an unsaved library.

use std::time::Duration;

use pitagames_core::i18n::t;
use pitagames_core::{Game, GameId, OwnedGame, Profile, Store};
use pitagames_db::{
    Db, GameRepository, HistoryRepository, LaunchRecord, MetadataRepository, OwnedRepository,
    ProfileRepository, StoredLaunch, StoredMetadata, StoredProfile,
};
use time::OffsetDateTime;

/// Open (or create) `<XDG_DATA_HOME>/pitagames/pitagames.db`.
pub async fn open_default() -> Result<Db, String> {
    let dir =
        pitagames_core::paths::data_dir().ok_or_else(|| t("runtime.no_data_dir").to_string())?;
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

/// Record a finished session: playtime accumulates, `last_played` moves.
///
/// A store that counts playtime itself (Steam, through `localconfig.vdf`) may
/// overrule this number on the next scan; a non-Steam game has no such counter,
/// so the sum of its sessions is the only playtime it will ever have.
pub async fn record_playtime(db: &Db, id: &GameId, playtime: Duration) -> Result<(), String> {
    db.set_playtime(id, playtime, Some(OffsetDateTime::now_utc()))
        .await
        .map(|_| ())
        .map_err(|error| format!("could not save playtime for {}: {error}", id.store_id))
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

/// The cached owned-games list and when it was fetched.
pub async fn load_owned(db: &Db) -> Result<(Vec<OwnedGame>, Option<OffsetDateTime>), String> {
    let games = db
        .owned_games(Store::Steam)
        .await
        .map_err(|error| format!("could not read the owned-games cache: {error}"))?;
    let fetched_at = db
        .owned_fetched_at(Store::Steam)
        .await
        .map_err(|error| format!("could not read the owned-games timestamp: {error}"))?;
    Ok((games, fetched_at))
}

/// Replace the cached owned-games list with a fresh API answer.
pub async fn save_owned(
    db: &Db,
    games: &[OwnedGame],
    fetched_at: OffsetDateTime,
) -> Result<usize, String> {
    db.replace_owned(Store::Steam, games, fetched_at)
        .await
        .map_err(|error| format!("could not save the owned-games cache: {error}"))
}

/// Drop the cached owned list (used when the key is forgotten).
pub async fn clear_owned(db: &Db) -> Result<usize, String> {
    db.remove_owned(Store::Steam)
        .await
        .map_err(|error| format!("could not clear the owned-games cache: {error}"))
}

/// The non-Steam rows a previous library sync wrote, in store order.
///
/// These are never scanned: `pitagames_stores::sync` is their only writer, and a
/// store whose CLI is missing simply keeps what the cache holds.
pub async fn load_store_rows(db: &Db) -> Result<Vec<Game>, String> {
    let mut rows = Vec::new();
    for store in pitagames_stores::status::CLI_STORES {
        let mut found = db
            .store_games(store)
            .await
            .map_err(|error| format!("could not read the {store} rows: {error}"))?;
        rows.append(&mut found);
    }
    Ok(rows)
}

/// Remember an install the launcher finished, so the next start shows it
/// without waiting for a store sync.
pub async fn record_install(db: &Db, game: &Game) -> Result<(), String> {
    db.upsert_game(game).await.map_err(|error| {
        format!(
            "could not save the install of {}: {error}",
            game.id.store_id
        )
    })
}

/// Append one finished session to the launch history.
///
/// The row is written whether the game ran or never started: a failed launch is
/// the row a user opens the history for. The write is best-effort like every
/// other persistence call — a full disk costs the row, never the verdict the
/// header strip already shows.
pub async fn record_launch_session(db: &Db, record: &LaunchRecord) -> Result<(), String> {
    db.add_launch(record).await.map(|_| ()).map_err(|error| {
        format!(
            "could not record the launch of {}: {error}",
            record.game.store_id
        )
    })
}

/// The newest sessions across every game, for the History screen.
pub async fn recent_launches(db: &Db, limit: usize) -> Result<Vec<StoredLaunch>, String> {
    db.launches(limit)
        .await
        .map_err(|error| format!("could not read the launch history: {error}"))
}
