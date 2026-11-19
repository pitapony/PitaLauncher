//! SQLite persistence for PitaGames.
//!
//! The database is a cache of discovered games plus the user's own profiles,
//! never the source of truth for a store. It is safe to delete: the next scan
//! rebuilds the games table.
//!
//! Queries are runtime-checked on purpose (no `query!` macros): the GUI ships
//! as a binary, and CI must not require a live database to compile.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use pitagames_core::{AntiCheat, Game, GameId, GameMetadata, OwnedGame, Profile, Store};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

/// Embedded migrations, applied in order on every open.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A pool plus the migration state.
///
/// Cloning is cheap: every clone shares the same SQLite pool.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (or create) a database file and apply migrations.
    ///
    /// Parent directories are created, WAL is enabled, and foreign keys are
    /// enforced. The path is expected to be `<XDG_DATA_HOME>/pitagames/pitagames.db`.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    /// A single-connection in-memory database, for tests and `--dry-run`.
    ///
    /// SQLite gives each `:memory:` connection its own database, so the pool is
    /// pinned to one connection that is never reaped.
    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;

        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    /// Apply pending migrations. Calling it twice is a no-op.
    pub async fn migrate(&self) -> Result<()> {
        match MIGRATOR.run(&self.pool).await {
            Ok(()) => Ok(()),
            // Two processes can open a database whose new migration is still
            // pending — the GUI and `pitagames-cli`, usually after an upgrade.
            // SQLite has no advisory lock, so both read the same pending
            // version list; one wins the DDL and the other's `CREATE TABLE`
            // fails with "already exists". Re-reading the applied versions
            // makes the loser a no-op instead of a failed startup.
            Err(sqlx::migrate::MigrateError::Execute(source))
            | Err(sqlx::migrate::MigrateError::ExecuteMigration(source, _))
                if is_duplicate_schema(&source) =>
            {
                MIGRATOR.run(&self.pool).await?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Escape hatch for components that need SQL the repositories do not cover.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// A profile as stored, with its database identity and timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProfile {
    pub id: i64,
    pub profile: Profile,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One cached metadata row.
///
/// `metadata == None` is a negative cache entry: the store was asked and had no
/// data for this app. `fetched_at` is when the answer was received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMetadata {
    pub game: GameId,
    pub metadata: Option<GameMetadata>,
    pub fetched_at: OffsetDateTime,
}

/// One supervised game session, as the history table stores it.
///
/// Steam launches its own games, so only the hand-off's sessions land here. A
/// session is recorded when it ends *and* when it could not be started: a
/// refusal ("no Proton build") is the most useful row in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRecord {
    pub game: GameId,
    /// Copied from the row at launch time: a history entry outlives the store's
    /// listing, so it must render after the game is uninstalled.
    pub title: String,
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
    /// Wall clock measured by the hand-off; zero when nothing was spawned.
    pub duration: Duration,
    /// `"umu"`, `"native"` or `"client"` — the hand-off's label for the launch
    /// shape; `None` when the launch failed before a shape was chosen.
    pub mode: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub succeeded: bool,
    /// The run's transcript under `<data>/pitagames/logs/`, if one was written.
    pub log_path: Option<PathBuf>,
    /// A one-line verdict: the success summary or the failure message.
    pub detail: Option<String>,
}

/// One history row with its database identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLaunch {
    pub id: i64,
    pub record: LaunchRecord,
}

/// What one store's row replacement did, for a sync report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryChange {
    /// Rows inserted or updated.
    pub written: usize,
    /// Rows dropped because the store's listing no longer names them.
    pub removed: usize,
}

/// Game rows, keyed by `(store, store_id)`.
#[async_trait]
pub trait GameRepository {
    /// Insert or update one game.
    ///
    /// Playtime and `last_played` are preserved on update: they come from
    /// [`GameRepository::set_playtime`], not from a scan.
    async fn upsert_game(&self, game: &Game) -> Result<()>;

    /// Insert or update many games in one transaction. Returns the number of
    /// written rows.
    async fn upsert_games(&self, games: &[Game]) -> Result<usize>;

    async fn game(&self, id: &GameId) -> Result<Option<Game>>;

    /// All games, ordered case-insensitively by title.
    async fn games(&self) -> Result<Vec<Game>>;

    /// One store's cached rows, ordered case-insensitively by title.
    ///
    /// This is what a library sync merges its fresh listing over.
    async fn store_games(&self, store: Store) -> Result<Vec<Game>>;

    /// Replace one store's cached rows with `games`, in one transaction.
    ///
    /// Rows of every other store are untouched: a sync only ever speaks for the
    /// store whose clients it ran. Rows missing from `games` are deleted, and
    /// the playtime/`last_played` of rows that survive are preserved — they come
    /// from the store's own data, not from a listing.
    async fn replace_store_games(&self, store: Store, games: &[Game]) -> Result<LibraryChange>;

    /// Replace the playtime. `None` for `last_played` keeps the stored value.
    async fn set_playtime(
        &self,
        id: &GameId,
        playtime: Duration,
        last_played: Option<OffsetDateTime>,
    ) -> Result<bool>;

    async fn remove_game(&self, id: &GameId) -> Result<bool>;
}

/// Named launch profiles, keyed by `(store, store_id, name)`.
#[async_trait]
pub trait ProfileRepository {
    /// Validate and save a profile, returning its row id. Saving the same
    /// `(game, name)` again updates in place and keeps the id.
    async fn save_profile(&self, profile: &Profile) -> Result<i64>;

    async fn profile(&self, game: &GameId, name: &str) -> Result<Option<StoredProfile>>;

    /// All profiles for a game, ordered case-insensitively by name.
    async fn profiles(&self, game: &GameId) -> Result<Vec<StoredProfile>>;

    async fn delete_profile(&self, game: &GameId, name: &str) -> Result<bool>;
}

/// The store metadata cache, keyed by `(store, store_id)`.
#[async_trait]
pub trait MetadataRepository {
    /// Insert or replace one row.
    async fn save_metadata(&self, row: &StoredMetadata) -> Result<()>;

    /// Insert or replace many rows in one transaction. Returns the row count.
    async fn save_metadata_many(&self, rows: &[StoredMetadata]) -> Result<usize>;

    async fn metadata(&self, game: &GameId) -> Result<Option<StoredMetadata>>;

    /// Every cached row, ordered by store and id.
    async fn metadata_rows(&self) -> Result<Vec<StoredMetadata>>;

    async fn remove_metadata(&self, game: &GameId) -> Result<bool>;
}

/// The owned-games cache: what an account owns, installed or not.
///
/// One account per store for now, so rows are scoped by store and not by
/// SteamID; a future multi-account view will add the account column with a
/// migration rather than guessing now.
#[async_trait]
pub trait OwnedRepository {
    /// Replace the whole cached list for `store` in one transaction.
    ///
    /// A purchase adds a row, a refund removes one; replacing (rather than
    /// upserting) is what keeps the cache equal to the last API answer.
    /// Returns the number of rows written.
    async fn replace_owned(
        &self,
        store: Store,
        games: &[OwnedGame],
        fetched_at: OffsetDateTime,
    ) -> Result<usize>;

    /// The cached list, ordered case-insensitively by title.
    async fn owned_games(&self, store: Store) -> Result<Vec<OwnedGame>>;

    /// When the cache was last replaced; `None` when it has never been filled.
    async fn owned_fetched_at(&self, store: Store) -> Result<Option<OffsetDateTime>>;

    /// Drop the cached list. Returns how many rows were removed.
    async fn remove_owned(&self, store: Store) -> Result<usize>;
}

/// The launch history: one row per supervised session, newest first when read.
#[async_trait]
pub trait HistoryRepository {
    /// Append one finished session, returning its row id.
    ///
    /// Records are append-only: a session is a fact, so nothing here updates or
    /// rewrites an existing row.
    async fn add_launch(&self, record: &LaunchRecord) -> Result<i64>;

    /// The newest sessions across every game, at most `limit`.
    async fn launches(&self, limit: usize) -> Result<Vec<StoredLaunch>>;

    /// The newest sessions for one game, at most `limit`.
    async fn launches_for(&self, game: &GameId, limit: usize) -> Result<Vec<StoredLaunch>>;

    /// Delete the history, all of it or one game's.
    ///
    /// Returns how many rows were removed. The GUI never calls this; it is the
    /// CLI's `history --clear`, so a user can forget a session without deleting
    /// the database the library lives in.
    async fn clear_launches(&self, game: Option<&GameId>) -> Result<usize>;
}

const UPSERT_GAME: &str = r#"
INSERT INTO games (
    store, store_id, title, install_dir, exe, installed, size_bytes,
    version, last_updated, playtime_secs, last_played, anticheat, tags, last_seen
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT (store, store_id) DO UPDATE SET
    title        = excluded.title,
    install_dir  = excluded.install_dir,
    exe          = excluded.exe,
    installed    = excluded.installed,
    size_bytes   = excluded.size_bytes,
    version      = excluded.version,
    last_updated = excluded.last_updated,
    anticheat    = excluded.anticheat,
    tags         = excluded.tags,
    last_seen    = excluded.last_seen
"#;

const SELECT_GAME: &str = r#"
SELECT store, store_id, title, install_dir, exe, installed, size_bytes,
       version, last_updated, playtime_secs, last_played, anticheat, tags
FROM games
"#;

/// Bind one `UPSERT_GAME` onto any executor (pool, connection or transaction).
///
/// Playtime and `last_played` are deliberately absent from the `DO UPDATE` set:
/// a store listing never knows them, so a rescan must not wipe them.
async fn bind_upsert<'e, E>(executor: E, game: &Game, now: i64) -> Result<usize>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let tags = serde_json::to_string(&game.tags)?;
    let result = sqlx::query(UPSERT_GAME)
        .bind(game.id.store.as_str())
        .bind(&game.id.store_id)
        .bind(&game.title)
        .bind(game.install_dir.as_deref().map(path_text))
        .bind(game.exe.as_deref().map(path_text))
        .bind(game.installed)
        .bind(game.size_bytes.map(|value| value as i64))
        .bind(game.version.as_deref())
        .bind(game.last_updated.map(OffsetDateTime::unix_timestamp))
        .bind(game.playtime.as_secs() as i64)
        .bind(game.last_played.map(OffsetDateTime::unix_timestamp))
        .bind(game.anticheat.as_str())
        .bind(tags)
        .bind(now)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() as usize)
}

#[async_trait]
impl GameRepository for Db {
    async fn upsert_game(&self, game: &Game) -> Result<()> {
        self.upsert_games(std::slice::from_ref(game)).await?;
        Ok(())
    }

    async fn upsert_games(&self, games: &[Game]) -> Result<usize> {
        let now = now_seconds();
        let mut tx = self.pool.begin().await?;
        let mut written = 0usize;

        for game in games {
            written += bind_upsert(&mut *tx, game, now).await?;
        }

        tx.commit().await?;
        Ok(written)
    }

    async fn game(&self, id: &GameId) -> Result<Option<Game>> {
        let sql = format!("{SELECT_GAME} WHERE store = ? AND store_id = ?");
        let row = sqlx::query(&sql)
            .bind(id.store.as_str())
            .bind(&id.store_id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(game_from_row).transpose()
    }

    async fn games(&self) -> Result<Vec<Game>> {
        let sql = format!("{SELECT_GAME} ORDER BY title COLLATE NOCASE, store, store_id");
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(game_from_row).collect()
    }

    async fn store_games(&self, store: Store) -> Result<Vec<Game>> {
        let sql = format!("{SELECT_GAME} WHERE store = ? ORDER BY title COLLATE NOCASE, store_id");
        let rows = sqlx::query(&sql)
            .bind(store.as_str())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(game_from_row).collect()
    }

    async fn replace_store_games(&self, store: Store, games: &[Game]) -> Result<LibraryChange> {
        if let Some(foreign) = games.iter().find(|game| game.id.store != store) {
            return Err(Error::Data {
                column: "games.store",
                message: format!(
                    "cannot cache a {} row while replacing {}",
                    foreign.id.store, store
                ),
            });
        }

        let now = now_seconds();
        let mut tx = self.pool.begin().await?;
        let mut change = LibraryChange::default();

        for game in games {
            change.written += bind_upsert(&mut *tx, game, now).await?;
        }

        let cached: Vec<String> = sqlx::query_scalar("SELECT store_id FROM games WHERE store = ?")
            .bind(store.as_str())
            .fetch_all(&mut *tx)
            .await?;
        for store_id in cached {
            if games.iter().any(|game| game.id.store_id == store_id) {
                continue;
            }
            let result = sqlx::query("DELETE FROM games WHERE store = ? AND store_id = ?")
                .bind(store.as_str())
                .bind(&store_id)
                .execute(&mut *tx)
                .await?;
            change.removed += result.rows_affected() as usize;
        }

        tx.commit().await?;
        Ok(change)
    }

    async fn set_playtime(
        &self,
        id: &GameId,
        playtime: Duration,
        last_played: Option<OffsetDateTime>,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE games
             SET playtime_secs = ?, last_played = COALESCE(?, last_played)
             WHERE store = ? AND store_id = ?",
        )
        .bind(playtime.as_secs() as i64)
        .bind(last_played.map(OffsetDateTime::unix_timestamp))
        .bind(id.store.as_str())
        .bind(&id.store_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn remove_game(&self, id: &GameId) -> Result<bool> {
        let result = sqlx::query("DELETE FROM games WHERE store = ? AND store_id = ?")
            .bind(id.store.as_str())
            .bind(&id.store_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait]
impl ProfileRepository for Db {
    async fn save_profile(&self, profile: &Profile) -> Result<i64> {
        // The database refuses profiles that could not be compiled safely.
        profile.validate()?;
        let json = serde_json::to_string(profile)?;
        let now = now_seconds();

        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO profiles (store, store_id, name, profile_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (store, store_id, name) DO UPDATE SET
                 profile_json = excluded.profile_json,
                 updated_at   = excluded.updated_at
             RETURNING id",
        )
        .bind(profile.game.store.as_str())
        .bind(&profile.game.store_id)
        .bind(&profile.name)
        .bind(json)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        Ok(id)
    }

    async fn profile(&self, game: &GameId, name: &str) -> Result<Option<StoredProfile>> {
        let row = sqlx::query(
            "SELECT id, store, store_id, name, profile_json, created_at, updated_at
             FROM profiles
             WHERE store = ? AND store_id = ? AND name = ?",
        )
        .bind(game.store.as_str())
        .bind(&game.store_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        row.as_ref().map(profile_from_row).transpose()
    }

    async fn profiles(&self, game: &GameId) -> Result<Vec<StoredProfile>> {
        let rows = sqlx::query(
            "SELECT id, store, store_id, name, profile_json, created_at, updated_at
             FROM profiles
             WHERE store = ? AND store_id = ?
             ORDER BY name COLLATE NOCASE",
        )
        .bind(game.store.as_str())
        .bind(&game.store_id)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(profile_from_row).collect()
    }

    async fn delete_profile(&self, game: &GameId, name: &str) -> Result<bool> {
        let result =
            sqlx::query("DELETE FROM profiles WHERE store = ? AND store_id = ? AND name = ?")
                .bind(game.store.as_str())
                .bind(&game.store_id)
                .bind(name)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait]
impl MetadataRepository for Db {
    async fn save_metadata(&self, row: &StoredMetadata) -> Result<()> {
        self.save_metadata_many(std::slice::from_ref(row)).await?;
        Ok(())
    }

    async fn save_metadata_many(&self, rows: &[StoredMetadata]) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        let mut written = 0usize;

        for row in rows {
            let payload = row
                .metadata
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let result = sqlx::query(
                "INSERT INTO game_metadata (store, store_id, payload_json, fetched_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT (store, store_id) DO UPDATE SET
                     payload_json = excluded.payload_json,
                     fetched_at   = excluded.fetched_at",
            )
            .bind(row.game.store.as_str())
            .bind(&row.game.store_id)
            .bind(payload)
            .bind(row.fetched_at.unix_timestamp())
            .execute(&mut *tx)
            .await?;
            written += result.rows_affected() as usize;
        }

        tx.commit().await?;
        Ok(written)
    }

    async fn metadata(&self, game: &GameId) -> Result<Option<StoredMetadata>> {
        let row = sqlx::query(
            "SELECT store, store_id, payload_json, fetched_at
             FROM game_metadata
             WHERE store = ? AND store_id = ?",
        )
        .bind(game.store.as_str())
        .bind(&game.store_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(metadata_from_row).transpose()
    }

    async fn metadata_rows(&self) -> Result<Vec<StoredMetadata>> {
        let rows = sqlx::query(
            "SELECT store, store_id, payload_json, fetched_at
             FROM game_metadata
             ORDER BY store, store_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(metadata_from_row).collect()
    }

    async fn remove_metadata(&self, game: &GameId) -> Result<bool> {
        let result = sqlx::query("DELETE FROM game_metadata WHERE store = ? AND store_id = ?")
            .bind(game.store.as_str())
            .bind(&game.store_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait]
impl OwnedRepository for Db {
    async fn replace_owned(
        &self,
        store: Store,
        games: &[OwnedGame],
        fetched_at: OffsetDateTime,
    ) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM owned_games WHERE store = ?")
            .bind(store.as_str())
            .execute(&mut *tx)
            .await?;

        let mut written = 0usize;
        for game in games {
            let result = sqlx::query(
                "INSERT INTO owned_games (store, store_id, title, playtime_secs, last_played, fetched_at)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (store, store_id) DO UPDATE SET
                     title         = excluded.title,
                     playtime_secs = excluded.playtime_secs,
                     last_played   = excluded.last_played,
                     fetched_at    = excluded.fetched_at",
            )
            .bind(game.id.store.as_str())
            .bind(&game.id.store_id)
            .bind(&game.title)
            .bind(game.playtime.as_secs() as i64)
            .bind(game.last_played.map(OffsetDateTime::unix_timestamp))
            .bind(fetched_at.unix_timestamp())
            .execute(&mut *tx)
            .await?;
            written += result.rows_affected() as usize;
        }

        tx.commit().await?;
        Ok(written)
    }

    async fn owned_games(&self, store: Store) -> Result<Vec<OwnedGame>> {
        let rows = sqlx::query(
            "SELECT store, store_id, title, playtime_secs, last_played
             FROM owned_games
             WHERE store = ?
             ORDER BY title COLLATE NOCASE, store_id",
        )
        .bind(store.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(owned_from_row).collect()
    }

    async fn owned_fetched_at(&self, store: Store) -> Result<Option<OffsetDateTime>> {
        let fetched_at: Option<i64> =
            sqlx::query_scalar("SELECT MAX(fetched_at) FROM owned_games WHERE store = ?")
                .bind(store.as_str())
                .fetch_one(&self.pool)
                .await?;
        timestamp(fetched_at, "owned_games.fetched_at")
    }

    async fn remove_owned(&self, store: Store) -> Result<usize> {
        let result = sqlx::query("DELETE FROM owned_games WHERE store = ?")
            .bind(store.as_str())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() as usize)
    }
}

const LAUNCH_COLUMNS: &str = "id, store, store_id, title, started_at, ended_at,
     duration_secs, mode, exit_code, signal, succeeded, log_path, detail";

#[async_trait]
impl HistoryRepository for Db {
    async fn add_launch(&self, record: &LaunchRecord) -> Result<i64> {
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO launch_history (
                 store, store_id, title, started_at, ended_at, duration_secs,
                 mode, exit_code, signal, succeeded, log_path, detail
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(record.game.store.as_str())
        .bind(&record.game.store_id)
        .bind(&record.title)
        .bind(record.started_at.unix_timestamp())
        .bind(record.ended_at.unix_timestamp())
        .bind(record.duration.as_secs() as i64)
        .bind(record.mode.as_deref())
        .bind(record.exit_code)
        .bind(record.signal)
        .bind(record.succeeded)
        .bind(record.log_path.as_deref().map(path_text))
        .bind(record.detail.as_deref())
        .fetch_one(&self.pool)
        .await?;

        Ok(id)
    }

    async fn launches(&self, limit: usize) -> Result<Vec<StoredLaunch>> {
        let sql = format!(
            "SELECT {LAUNCH_COLUMNS} FROM launch_history
             ORDER BY started_at DESC, id DESC LIMIT ?"
        );
        let rows = sqlx::query(&sql)
            .bind(limit_to_i64(limit))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(launch_from_row).collect()
    }

    async fn launches_for(&self, game: &GameId, limit: usize) -> Result<Vec<StoredLaunch>> {
        let sql = format!(
            "SELECT {LAUNCH_COLUMNS} FROM launch_history
             WHERE store = ? AND store_id = ?
             ORDER BY started_at DESC, id DESC LIMIT ?"
        );
        let rows = sqlx::query(&sql)
            .bind(game.store.as_str())
            .bind(&game.store_id)
            .bind(limit_to_i64(limit))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(launch_from_row).collect()
    }

    async fn clear_launches(&self, game: Option<&GameId>) -> Result<usize> {
        let result = match game {
            Some(game) => {
                sqlx::query("DELETE FROM launch_history WHERE store = ? AND store_id = ?")
                    .bind(game.store.as_str())
                    .bind(&game.store_id)
                    .execute(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("DELETE FROM launch_history")
                    .execute(&self.pool)
                    .await?
            }
        };
        Ok(result.rows_affected() as usize)
    }
}

/// `LIMIT` takes a signed integer; saturating is right for a display cap.
fn limit_to_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

fn launch_from_row(row: &SqliteRow) -> Result<StoredLaunch> {
    let store: String = row.try_get("store")?;
    let store_id: String = row.try_get("store_id")?;
    let duration_secs: i64 = row.try_get("duration_secs")?;
    let succeeded: i64 = row.try_get("succeeded")?;
    let exit_code: Option<i64> = row.try_get("exit_code")?;
    let signal: Option<i64> = row.try_get("signal")?;

    Ok(StoredLaunch {
        id: row.try_get("id")?,
        record: LaunchRecord {
            game: GameId::new(Store::from_str(&store)?, store_id),
            title: row.try_get("title")?,
            started_at: required_timestamp(row.try_get("started_at")?, "started_at")?,
            ended_at: required_timestamp(row.try_get("ended_at")?, "ended_at")?,
            duration: Duration::from_secs(u64::try_from(duration_secs).map_err(|_| {
                Error::Data {
                    column: "launch_history.duration_secs",
                    message: format!("negative value {duration_secs}"),
                }
            })?),
            mode: row.try_get("mode")?,
            exit_code: exit_code
                .map(|code| {
                    i32::try_from(code).map_err(|_| Error::Data {
                        column: "launch_history.exit_code",
                        message: format!("out of range: {code}"),
                    })
                })
                .transpose()?,
            signal: signal
                .map(|value| {
                    i32::try_from(value).map_err(|_| Error::Data {
                        column: "launch_history.signal",
                        message: format!("out of range: {value}"),
                    })
                })
                .transpose()?,
            succeeded: succeeded != 0,
            log_path: row
                .try_get::<Option<String>, _>("log_path")?
                .map(PathBuf::from),
            detail: row.try_get("detail")?,
        },
    })
}

fn owned_from_row(row: &SqliteRow) -> Result<OwnedGame> {
    let store: String = row.try_get("store")?;
    let store_id: String = row.try_get("store_id")?;
    let playtime_secs: i64 = row.try_get("playtime_secs")?;

    Ok(OwnedGame {
        id: GameId::new(Store::from_str(&store)?, store_id),
        title: row.try_get("title")?,
        playtime: Duration::from_secs(u64::try_from(playtime_secs).map_err(|_| Error::Data {
            column: "owned_games.playtime_secs",
            message: format!("negative value {playtime_secs}"),
        })?),
        last_played: timestamp(row.try_get("last_played")?, "owned_games.last_played")?,
    })
}

fn game_from_row(row: &SqliteRow) -> Result<Game> {
    let store: String = row.try_get("store")?;
    let store_id: String = row.try_get("store_id")?;
    let title: String = row.try_get("title")?;
    let installed: i64 = row.try_get("installed")?;
    let size_bytes: Option<i64> = row.try_get("size_bytes")?;
    let playtime_secs: i64 = row.try_get("playtime_secs")?;
    let anticheat: String = row.try_get("anticheat")?;
    let tags: String = row.try_get("tags")?;

    Ok(Game {
        id: GameId::new(Store::from_str(&store)?, store_id),
        title,
        install_dir: row
            .try_get::<Option<String>, _>("install_dir")?
            .map(PathBuf::from),
        exe: row.try_get::<Option<String>, _>("exe")?.map(PathBuf::from),
        installed: installed != 0,
        size_bytes: size_bytes
            .map(|value| {
                u64::try_from(value).map_err(|_| Error::Data {
                    column: "size_bytes",
                    message: format!("negative value {value}"),
                })
            })
            .transpose()?,
        version: row.try_get("version")?,
        last_updated: timestamp(row.try_get("last_updated")?, "last_updated")?,
        last_played: timestamp(row.try_get("last_played")?, "last_played")?,
        playtime: Duration::from_secs(u64::try_from(playtime_secs).map_err(|_| Error::Data {
            column: "playtime_secs",
            message: format!("negative value {playtime_secs}"),
        })?),
        anticheat: AntiCheat::from_str(&anticheat)?,
        tags: serde_json::from_str(&tags).map_err(|source| Error::Data {
            column: "tags",
            message: source.to_string(),
        })?,
    })
}

fn metadata_from_row(row: &SqliteRow) -> Result<StoredMetadata> {
    let store: String = row.try_get("store")?;
    let store_id: String = row.try_get("store_id")?;
    let payload_json: Option<String> = row.try_get("payload_json")?;

    let metadata: Option<GameMetadata> = payload_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|source| Error::Data {
            column: "game_metadata.payload_json",
            message: source.to_string(),
        })?;

    let expected = GameId::new(Store::from_str(&store)?, store_id);
    if let Some(metadata) = &metadata
        && metadata.game != expected
    {
        return Err(Error::Data {
            column: "game_metadata.payload_json",
            message: format!(
                "payload identifies {} / {:?}, row is {} / {:?}",
                metadata.game.store, metadata.game.store_id, expected.store, expected.store_id
            ),
        });
    }

    Ok(StoredMetadata {
        game: expected,
        metadata,
        fetched_at: required_timestamp(row.try_get("fetched_at")?, "fetched_at")?,
    })
}

fn profile_from_row(row: &SqliteRow) -> Result<StoredProfile> {
    let profile_json: String = row.try_get("profile_json")?;
    let profile: Profile = serde_json::from_str(&profile_json).map_err(|source| Error::Data {
        column: "profile_json",
        message: source.to_string(),
    })?;

    let store: String = row.try_get("store")?;
    let store_id: String = row.try_get("store_id")?;
    let name: String = row.try_get("name")?;
    // Catch a row/key mismatch before a launch uses the wrong profile.
    let expected = GameId::new(Store::from_str(&store)?, store_id);
    if profile.game != expected || profile.name != name {
        return Err(Error::Data {
            column: "profile_json",
            message: format!(
                "payload identifies {} / {:?}, row is {} / {:?}",
                profile.game.store, profile.game.store_id, expected.store, name
            ),
        });
    }

    Ok(StoredProfile {
        id: row.try_get("id")?,
        profile,
        created_at: required_timestamp(row.try_get("created_at")?, "created_at")?,
        updated_at: required_timestamp(row.try_get("updated_at")?, "updated_at")?,
    })
}

fn timestamp(value: Option<i64>, column: &'static str) -> Result<Option<OffsetDateTime>> {
    value
        .map(|seconds| {
            OffsetDateTime::from_unix_timestamp(seconds).map_err(|source| Error::Data {
                column,
                message: source.to_string(),
            })
        })
        .transpose()
}

fn required_timestamp(value: i64, column: &'static str) -> Result<OffsetDateTime> {
    timestamp(Some(value), column).map(|value| value.expect("Some was passed"))
}

fn path_text(path: &Path) -> String {
    // Paths are TEXT because PathBuf's serde form is a string; see the
    // migration header. Non-UTF-8 paths are not a supported store layout.
    path.to_string_lossy().into_owned()
}

fn now_seconds() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Whether a migration failed because another connection already applied it.
///
/// The message is SQLite's own; the alternative is a retry on every failure,
/// which would hide a genuinely broken migration.
fn is_duplicate_schema(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(inner) if inner.message().contains("already exists")
    )
}

/// Database failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("failed to prepare database directory {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid {column} value in database: {message}")]
    Data {
        column: &'static str,
        message: String,
    },

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Profile(#[from] pitagames_core::ProfileError),

    #[error(transparent)]
    UnknownStore(#[from] pitagames_core::UnknownStore),

    #[error(transparent)]
    UnknownAntiCheat(#[from] pitagames_core::UnknownAntiCheat),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{
        Diagnostics, FrameGen, GamescopeOptions, Injection, InjectionDll, ModelSource,
        NeuralRendering, NrBackend, NrModel, NrProcessing, NrRoute, OwnedGame, PkgArch, ProtonRef,
        Upscaler, WrapperChain, WrapperStep,
    };
    use std::collections::BTreeMap;

    fn steam_game(app_id: &str, title: &str) -> Game {
        Game::new(GameId::new(Store::Steam, app_id), title)
    }

    fn fully_populated_game() -> Game {
        let mut game = Game::new(GameId::new(Store::Epic, "epic-123"), "Ünïcode Game");
        game.install_dir = Some(PathBuf::from("/games/Foo Bar"));
        game.exe = Some(PathBuf::from("/games/Foo Bar/bin/game.exe"));
        game.installed = true;
        game.size_bytes = Some(12_345_678_901);
        game.version = Some("1.2.3".into());
        game.last_updated = Some(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());
        game.last_played = Some(OffsetDateTime::from_unix_timestamp(1_710_000_000).unwrap());
        game.playtime = Duration::from_secs(3_600);
        game.anticheat = AntiCheat::Eac;
        game.tags = vec!["coop".into(), "hdr".into(), "中文".into()];
        game
    }

    fn nr_profile() -> Profile {
        let mut profile = Profile::new(GameId::new(Store::Steam, "620"), "FSR 4 + NR");
        profile.proton = ProtonRef::GeProton;
        profile.upscaler = Upscaler::Native;
        profile.frame_gen = FrameGen::Fsr4Mlfg;
        profile.injection = Injection::DlssNr;
        profile.injection_dll = InjectionDll::Winmm;
        profile.neural_rendering = NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.8".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 50,
            processing: NrProcessing::RenoDx,
            pkg_arch: PkgArch::X86_64,
            captured_env: BTreeMap::from([(
                "WINEDLLOVERRIDES".to_string(),
                "dxgi=n,b".to_string(),
            )]),
            model_source: ModelSource::ScannedDll {
                path: PathBuf::from("/games/nvngx_dlssnr.dll"),
            },
        };
        profile.wrappers = WrapperChain {
            gamemode: true,
            mangohud: true,
            gamescope: Some(GamescopeOptions {
                width: Some(1920),
                height: Some(1080),
                refresh_hz: Some(120),
                fullscreen: true,
                extra_args: vec!["--adaptive-sync".into()],
            }),
            custom: vec![WrapperStep {
                program: PathBuf::from("/usr/bin/mangohud"),
                args: vec!["--dls".into()],
            }],
        };
        profile.diagnostics = Diagnostics {
            dxvk_hud: true,
            proton_log: true,
            ..Diagnostics::default()
        };
        profile.env.insert("DXVK_HDR".into(), "1".into());
        profile
            .ini_overrides
            .insert("Upscalers.Dx12Upscaler".into(), "fsr4".into());
        profile
    }

    #[tokio::test]
    async fn games_round_trip_through_the_database() {
        let db = Db::open_in_memory().await.unwrap();
        let game = fully_populated_game();

        db.upsert_game(&game).await.unwrap();
        let loaded = db.game(&game.id).await.unwrap().unwrap();

        assert_eq!(loaded, game);
    }

    #[tokio::test]
    async fn upserting_preserves_playtime_until_explicitly_set() {
        let db = Db::open_in_memory().await.unwrap();
        let game = steam_game("620", "Portal 2");
        db.upsert_game(&game).await.unwrap();

        let played_at = OffsetDateTime::from_unix_timestamp(1_720_000_000).unwrap();
        assert!(
            db.set_playtime(&game.id, Duration::from_secs(120), Some(played_at))
                .await
                .unwrap()
        );

        // A later scan reports no playtime; it must not wipe the stored value.
        let mut rescanned = game.clone();
        rescanned.playtime = Duration::ZERO;
        rescanned.last_played = None;
        db.upsert_game(&rescanned).await.unwrap();

        let loaded = db.game(&game.id).await.unwrap().unwrap();
        assert_eq!(loaded.playtime, Duration::from_secs(120));
        assert_eq!(loaded.last_played, Some(played_at));

        // `None` for last_played keeps the previous timestamp.
        db.set_playtime(&game.id, Duration::from_secs(300), None)
            .await
            .unwrap();
        let loaded = db.game(&game.id).await.unwrap().unwrap();
        assert_eq!(loaded.playtime, Duration::from_secs(300));
        assert_eq!(loaded.last_played, Some(played_at));
    }

    #[tokio::test]
    async fn games_are_listed_case_insensitively_by_title() {
        let db = Db::open_in_memory().await.unwrap();
        for (id, title) in [("1", "zeta"), ("2", "Alpha"), ("3", "beta")] {
            db.upsert_game(&steam_game(id, title)).await.unwrap();
        }

        let titles: Vec<String> = db
            .games()
            .await
            .unwrap()
            .into_iter()
            .map(|game| game.title)
            .collect();
        assert_eq!(titles, ["Alpha", "beta", "zeta"]);
    }

    #[tokio::test]
    async fn upsert_many_is_one_transaction_and_counts_writes() {
        let db = Db::open_in_memory().await.unwrap();
        let games = vec![
            steam_game("620", "Portal 2"),
            steam_game("440", "Team Fortress 2"),
        ];

        assert_eq!(db.upsert_games(&games).await.unwrap(), 2);
        assert_eq!(db.games().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn removing_a_game_reports_whether_it_existed() {
        let db = Db::open_in_memory().await.unwrap();
        let game = steam_game("620", "Portal 2");
        db.upsert_game(&game).await.unwrap();

        assert!(db.remove_game(&game.id).await.unwrap());
        assert!(!db.remove_game(&game.id).await.unwrap());
        assert!(db.game(&game.id).await.unwrap().is_none());
    }

    fn epic_game(id: &str, title: &str) -> Game {
        Game::new(GameId::new(Store::Epic, id), title)
    }

    #[tokio::test]
    async fn replacing_a_stores_rows_writes_and_drops_in_one_shot() {
        let db = Db::open_in_memory().await.unwrap();
        db.replace_store_games(
            Store::Epic,
            &[epic_game("Gizmo", "Gizmo"), epic_game("Sugar", "Sugar")],
        )
        .await
        .unwrap();

        let mut gizmo = epic_game("Gizmo", "Gizmo");
        gizmo.installed = true;
        gizmo.install_dir = Some(PathBuf::from("/games/epic/Gizmo"));
        let change = db
            .replace_store_games(Store::Epic, &[gizmo.clone()])
            .await
            .unwrap();

        assert_eq!(
            change,
            LibraryChange {
                written: 1,
                removed: 1
            }
        );
        let rows = db.store_games(Store::Epic).await.unwrap();
        assert_eq!(rows, vec![gizmo]);
    }

    #[tokio::test]
    async fn replacing_one_store_never_touches_another() {
        let db = Db::open_in_memory().await.unwrap();
        db.upsert_games(&[steam_game("620", "Portal 2"), epic_game("Gizmo", "Gizmo")])
            .await
            .unwrap();

        let change = db
            .replace_store_games(Store::Epic, &[epic_game("Sugar", "Sugar")])
            .await
            .unwrap();

        assert_eq!(change.removed, 1);
        assert!(
            db.game(&GameId::new(Store::Steam, "620"))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.game(&GameId::new(Store::Epic, "Gizmo"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(db.store_games(Store::Epic).await.unwrap().len(), 1);
        assert_eq!(db.games().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_replaced_row_keeps_the_playtime_the_store_reported_earlier() {
        let db = Db::open_in_memory().await.unwrap();
        let game = epic_game("Gizmo", "Gizmo");
        db.upsert_game(&game).await.unwrap();
        let played_at = OffsetDateTime::from_unix_timestamp(1_720_000_000).unwrap();
        db.set_playtime(&game.id, Duration::from_secs(7200), Some(played_at))
            .await
            .unwrap();

        // The fresh listing carries no playtime; the resync must not zero it.
        db.replace_store_games(Store::Epic, std::slice::from_ref(&game))
            .await
            .unwrap();

        let loaded = db.game(&game.id).await.unwrap().unwrap();
        assert_eq!(loaded.playtime, Duration::from_secs(7200));
        assert_eq!(loaded.last_played, Some(played_at));
    }

    #[tokio::test]
    async fn an_empty_listing_clears_that_store_but_not_the_database() {
        let db = Db::open_in_memory().await.unwrap();
        db.upsert_games(&[steam_game("620", "Portal 2"), epic_game("Gizmo", "Gizmo")])
            .await
            .unwrap();

        let change = db.replace_store_games(Store::Epic, &[]).await.unwrap();
        assert_eq!(
            change,
            LibraryChange {
                written: 0,
                removed: 1
            }
        );
        assert!(db.store_games(Store::Epic).await.unwrap().is_empty());
        assert_eq!(db.games().await.unwrap().len(), 1);

        // Replacing again with the same empty set is a no-op.
        assert_eq!(
            db.replace_store_games(Store::Epic, &[]).await.unwrap(),
            LibraryChange::default()
        );
    }

    #[tokio::test]
    async fn a_foreign_store_row_is_refused_before_any_write() {
        let db = Db::open_in_memory().await.unwrap();
        db.replace_store_games(Store::Epic, &[epic_game("Gizmo", "Gizmo")])
            .await
            .unwrap();

        let error = db
            .replace_store_games(Store::Epic, &[steam_game("620", "Portal 2")])
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Data { column, .. } if column == "games.store"),
            "{error}"
        );
        assert_eq!(
            db.store_games(Store::Epic).await.unwrap(),
            vec![epic_game("Gizmo", "Gizmo")],
            "the refused transaction wrote nothing"
        );
    }

    #[tokio::test]
    async fn profiles_round_trip_and_keep_their_id_on_update() {
        let db = Db::open_in_memory().await.unwrap();
        let mut profile = nr_profile();

        let first_id = db.save_profile(&profile).await.unwrap();
        let stored = db
            .profile(&profile.game, &profile.name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.id, first_id);
        assert_eq!(stored.profile, profile);
        assert!(stored.updated_at >= stored.created_at);

        profile.frame_gen = FrameGen::Fsr3;
        let second_id = db.save_profile(&profile).await.unwrap();
        assert_eq!(second_id, first_id);
        assert_eq!(db.profiles(&profile.game).await.unwrap().len(), 1);
        assert_eq!(
            db.profile(&profile.game, &profile.name)
                .await
                .unwrap()
                .unwrap()
                .profile,
            profile
        );
    }

    #[tokio::test]
    async fn profiles_are_scoped_per_game_and_sorted_by_name() {
        let db = Db::open_in_memory().await.unwrap();
        let game = GameId::new(Store::Steam, "620");
        let mut zeta = Profile::new(game.clone(), "zeta");
        let alpha = Profile::new(game.clone(), "Alpha");
        let other = Profile::new(GameId::new(Store::Steam, "440"), "Default");

        db.save_profile(&zeta).await.unwrap();
        db.save_profile(&alpha).await.unwrap();
        db.save_profile(&other).await.unwrap();

        let names: Vec<String> = db
            .profiles(&game)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| stored.profile.name)
            .collect();
        assert_eq!(names, ["Alpha", "zeta"]);

        zeta.name = "renamed".into();
        // Renaming creates a second profile; it does not overwrite "zeta".
        db.save_profile(&zeta).await.unwrap();
        assert_eq!(db.profiles(&game).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn invalid_profiles_are_refused_before_they_hit_the_database() {
        let db = Db::open_in_memory().await.unwrap();
        let mut profile = nr_profile();
        let NeuralRendering::DlssNrAmd { model_scale, .. } = &mut profile.neural_rendering else {
            unreachable!();
        };
        *model_scale = 10;

        let error = db.save_profile(&profile).await.unwrap_err();
        assert!(matches!(
            error,
            Error::Profile(pitagames_core::ProfileError::ModelScale { .. })
        ));
        assert!(db.profiles(&profile.game).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_missing_profile_is_false() {
        let db = Db::open_in_memory().await.unwrap();
        let game = GameId::new(Store::Steam, "620");
        let profile = Profile::new(game.clone(), "Default");

        assert!(!db.delete_profile(&game, "Default").await.unwrap());
        db.save_profile(&profile).await.unwrap();
        assert!(db.delete_profile(&game, "Default").await.unwrap());
        assert!(db.profile(&game, "Default").await.unwrap().is_none());
    }

    fn sample_metadata(app_id: &str) -> GameMetadata {
        let mut metadata = GameMetadata::new(GameId::new(Store::Steam, app_id), "Portal 2");
        metadata.short_description = Some("Puzzles, but with friends.".into());
        metadata.developers = vec!["Valve".into()];
        metadata.publishers = vec!["Valve".into()];
        metadata.genres = vec!["Action".into(), "Adventure".into()];
        metadata.release_date = Some("Apr 18, 2011".into());
        metadata.platforms.linux = true;
        metadata.platforms.windows = true;
        metadata.metacritic = Some(95);
        metadata.header_image = Some("https://example.test/header.jpg".into());
        metadata
    }

    #[tokio::test]
    async fn metadata_round_trips_including_negative_entries() {
        let db = Db::open_in_memory().await.unwrap();
        let fetched_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();

        let positive = StoredMetadata {
            game: GameId::new(Store::Steam, "620"),
            metadata: Some(sample_metadata("620")),
            fetched_at,
        };
        let negative = StoredMetadata {
            game: GameId::new(Store::Steam, "999999999"),
            metadata: None,
            fetched_at,
        };

        db.save_metadata(&positive).await.unwrap();
        db.save_metadata(&negative).await.unwrap();

        assert_eq!(
            db.metadata(&positive.game).await.unwrap(),
            Some(positive.clone())
        );
        assert_eq!(
            db.metadata(&negative.game).await.unwrap(),
            Some(negative.clone())
        );
        assert!(
            db.metadata(&GameId::new(Store::Steam, "440"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(db.metadata_rows().await.unwrap(), vec![positive, negative]);
    }

    #[tokio::test]
    async fn metadata_upsert_replaces_the_payload_and_timestamp() {
        let db = Db::open_in_memory().await.unwrap();
        let game = GameId::new(Store::Steam, "620");
        let first = StoredMetadata {
            game: game.clone(),
            metadata: None,
            fetched_at: OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
        };
        db.save_metadata(&first).await.unwrap();

        let second = StoredMetadata {
            game: game.clone(),
            metadata: Some(sample_metadata("620")),
            fetched_at: OffsetDateTime::from_unix_timestamp(2_000).unwrap(),
        };
        db.save_metadata(&second).await.unwrap();

        assert_eq!(db.metadata(&game).await.unwrap(), Some(second));
        assert_eq!(db.metadata_rows().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn many_metadata_rows_are_one_transaction() {
        let db = Db::open_in_memory().await.unwrap();
        let fetched_at = OffsetDateTime::UNIX_EPOCH;
        let rows: Vec<StoredMetadata> = ["620", "440"]
            .iter()
            .map(|app_id| StoredMetadata {
                game: GameId::new(Store::Steam, *app_id),
                metadata: Some(sample_metadata(app_id)),
                fetched_at,
            })
            .collect();

        assert_eq!(db.save_metadata_many(&rows).await.unwrap(), 2);
        assert_eq!(db.metadata_rows().await.unwrap().len(), 2);

        assert!(db.remove_metadata(&rows[0].game).await.unwrap());
        assert!(!db.remove_metadata(&rows[0].game).await.unwrap());
        assert_eq!(db.metadata_rows().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn corrupt_metadata_is_reported_not_ignored() {
        let db = Db::open_in_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO game_metadata (store, store_id, payload_json, fetched_at)
             VALUES (?, ?, ?, 0)",
        )
        .bind("steam")
        .bind("620")
        .bind("{not json")
        .execute(db.pool())
        .await
        .unwrap();

        let error = db
            .metadata(&GameId::new(Store::Steam, "620"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Data { column, .. } if column == "game_metadata.payload_json"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn metadata_for_the_wrong_game_is_reported() {
        let db = Db::open_in_memory().await.unwrap();
        let payload = serde_json::to_string(&sample_metadata("440")).unwrap();
        sqlx::query(
            "INSERT INTO game_metadata (store, store_id, payload_json, fetched_at)
             VALUES (?, ?, ?, 0)",
        )
        .bind("steam")
        .bind("620")
        .bind(payload)
        .execute(db.pool())
        .await
        .unwrap();

        let error = db
            .metadata(&GameId::new(Store::Steam, "620"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Data { column, .. } if column == "game_metadata.payload_json"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn metadata_survives_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/pitagames.db");
        let game = GameId::new(Store::Steam, "620");
        let row = StoredMetadata {
            game: game.clone(),
            metadata: Some(sample_metadata("620")),
            fetched_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        };

        {
            let db = Db::open(&path).await.unwrap();
            db.save_metadata(&row).await.unwrap();
        }

        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.metadata(&game).await.unwrap(), Some(row));
    }

    #[tokio::test]
    async fn file_databases_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/pitagames.db");
        let game = steam_game("620", "Portal 2");

        {
            let db = Db::open(&path).await.unwrap();
            db.upsert_game(&game).await.unwrap();
            db.save_profile(&Profile::new(game.id.clone(), "Default"))
                .await
                .unwrap();
        }

        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.game(&game.id).await.unwrap().unwrap(), game);
        assert_eq!(db.games().await.unwrap().len(), 1);
        assert_eq!(db.profiles(&game.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn corrupt_rows_are_reported_not_ignored() {
        let db = Db::open_in_memory().await.unwrap();
        sqlx::query("INSERT INTO games (store, store_id, title, installed, tags, last_seen) VALUES (?, ?, ?, 1, ?, 0)")
            .bind("origin")
            .bind("1")
            .bind("Game")
            .bind("[]")
            .execute(db.pool())
            .await
            .unwrap();

        let error = db.games().await.unwrap_err();
        assert!(matches!(error, Error::UnknownStore(_)), "{error}");
    }

    fn owned(app_id: &str, title: &str, minutes: u64) -> OwnedGame {
        let mut game = OwnedGame::new(GameId::new(Store::Steam, app_id), title);
        game.playtime = Duration::from_secs(minutes * 60);
        game
    }

    #[tokio::test]
    async fn owned_games_round_trip_and_are_sorted() {
        let db = Db::open_in_memory().await.unwrap();
        let fetched_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut portal = owned("620", "portal 2", 412);
        portal.last_played = Some(OffsetDateTime::from_unix_timestamp(1_710_000_000).unwrap());

        let count = db
            .replace_owned(
                Store::Steam,
                &[
                    owned("570", "Dota 2", 0),
                    portal.clone(),
                    owned("400", "Half-Life", 5),
                ],
                fetched_at,
            )
            .await
            .unwrap();

        assert_eq!(count, 3);
        let games = db.owned_games(Store::Steam).await.unwrap();
        assert_eq!(
            games
                .iter()
                .map(|game| game.title.as_str())
                .collect::<Vec<_>>(),
            ["Dota 2", "Half-Life", "portal 2"]
        );
        assert_eq!(games[2], portal);
        assert_eq!(
            db.owned_fetched_at(Store::Steam).await.unwrap(),
            Some(fetched_at)
        );
        assert_eq!(
            db.owned_fetched_at(Store::Gog).await.unwrap(),
            None,
            "no cache for a store that was never fetched"
        );
    }

    #[tokio::test]
    async fn replacing_drops_games_no_longer_owned() {
        let db = Db::open_in_memory().await.unwrap();
        let first = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let second = OffsetDateTime::from_unix_timestamp(1_700_086_400).unwrap();

        db.replace_owned(
            Store::Steam,
            &[owned("620", "Portal 2", 10), owned("570", "Dota 2", 0)],
            first,
        )
        .await
        .unwrap();
        db.replace_owned(Store::Steam, &[owned("620", "Portal 2", 12)], second)
            .await
            .unwrap();

        let games = db.owned_games(Store::Steam).await.unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].playtime, Duration::from_secs(12 * 60));
        assert_eq!(
            db.owned_fetched_at(Store::Steam).await.unwrap(),
            Some(second)
        );
    }

    #[tokio::test]
    async fn an_empty_owned_list_clears_the_cache() {
        let db = Db::open_in_memory().await.unwrap();
        let fetched_at = OffsetDateTime::UNIX_EPOCH;
        db.replace_owned(Store::Steam, &[owned("620", "Portal 2", 10)], fetched_at)
            .await
            .unwrap();

        db.replace_owned(Store::Steam, &[], fetched_at)
            .await
            .unwrap();
        assert!(db.owned_games(Store::Steam).await.unwrap().is_empty());
        assert_eq!(db.owned_fetched_at(Store::Steam).await.unwrap(), None);

        db.replace_owned(Store::Steam, &[owned("620", "Portal 2", 10)], fetched_at)
            .await
            .unwrap();
        assert_eq!(db.remove_owned(Store::Steam).await.unwrap(), 1);
        assert_eq!(db.remove_owned(Store::Steam).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_owned_cache_is_scoped_per_store() {
        let db = Db::open_in_memory().await.unwrap();
        let fetched_at = OffsetDateTime::UNIX_EPOCH;
        let mut gog = owned("1234", "GOG Game", 0);
        gog.id.store = Store::Gog;

        db.replace_owned(Store::Steam, &[owned("620", "Portal 2", 0)], fetched_at)
            .await
            .unwrap();
        db.replace_owned(Store::Gog, &[gog], fetched_at)
            .await
            .unwrap();
        db.replace_owned(Store::Gog, &[], fetched_at).await.unwrap();

        assert_eq!(db.owned_games(Store::Steam).await.unwrap().len(), 1);
        assert!(db.owned_games(Store::Gog).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_corrupt_owned_row_is_reported() {
        let db = Db::open_in_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO owned_games (store, store_id, title, playtime_secs, fetched_at)
             VALUES ('steam', '620', 'Portal 2', -5, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let error = db.owned_games(Store::Steam).await.unwrap_err();
        assert!(
            matches!(error, Error::Data { column, .. } if column == "owned_games.playtime_secs"),
            "{error}"
        );
    }

    fn session(id: &str, at: i64, seconds: u64) -> LaunchRecord {
        LaunchRecord {
            game: GameId::new(Store::Epic, id),
            title: format!("Game {id}"),
            started_at: OffsetDateTime::from_unix_timestamp(at).unwrap(),
            ended_at: OffsetDateTime::from_unix_timestamp(at + seconds as i64).unwrap(),
            duration: Duration::from_secs(seconds),
            mode: Some("umu".to_string()),
            exit_code: Some(0),
            signal: None,
            succeeded: true,
            log_path: Some(PathBuf::from(format!("/logs/epic-{id}.log"))),
            detail: Some(format!("exited 0 after {seconds}s")),
        }
    }

    #[tokio::test]
    async fn a_launch_record_round_trips() {
        let db = Db::open_in_memory().await.unwrap();
        let record = session("Gizmo", 1_760_000_000, 3_725);
        let id = db.add_launch(&record).await.unwrap();
        assert!(id > 0);

        let stored = db.launches(10).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, id);
        assert_eq!(stored[0].record, record);
    }

    #[tokio::test]
    async fn a_failed_launch_keeps_its_unknowns_null() {
        let db = Db::open_in_memory().await.unwrap();
        let record = LaunchRecord {
            game: GameId::new(Store::Gog, "1234"),
            title: "GOG Game".to_string(),
            started_at: OffsetDateTime::UNIX_EPOCH,
            ended_at: OffsetDateTime::UNIX_EPOCH,
            duration: Duration::ZERO,
            mode: None,
            exit_code: None,
            signal: None,
            succeeded: false,
            log_path: None,
            detail: Some("no Proton build was found".to_string()),
        };
        db.add_launch(&record).await.unwrap();

        let stored = db.launches(10).await.unwrap();
        assert_eq!(stored[0].record, record);
    }

    #[tokio::test]
    async fn history_is_newest_first_and_scoped_per_game() {
        let db = Db::open_in_memory().await.unwrap();
        db.add_launch(&session("Gizmo", 1_760_000_000, 60))
            .await
            .unwrap();
        db.add_launch(&session("Sugar", 1_760_000_500, 30))
            .await
            .unwrap();
        db.add_launch(&session("Gizmo", 1_760_001_000, 90))
            .await
            .unwrap();

        let all: Vec<u64> = db
            .launches(10)
            .await
            .unwrap()
            .iter()
            .map(|row| row.record.duration.as_secs())
            .collect();
        assert_eq!(all, [90, 30, 60], "newest session first");

        let gizmo = db
            .launches_for(&GameId::new(Store::Epic, "Gizmo"), 10)
            .await
            .unwrap();
        assert_eq!(gizmo.len(), 2);
        assert_eq!(gizmo[0].record.duration, Duration::from_secs(90));

        // The cap is applied by the query, not by the caller.
        assert_eq!(db.launches(1).await.unwrap().len(), 1);
        assert_eq!(
            db.launches_for(&GameId::new(Store::Epic, "Gizmo"), 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn history_survives_the_game_row_and_can_be_cleared() {
        let db = Db::open_in_memory().await.unwrap();
        let game = epic_game("Gizmo", "Gizmo");
        db.upsert_game(&game).await.unwrap();
        db.add_launch(&session("Gizmo", 1_760_000_000, 60))
            .await
            .unwrap();
        db.add_launch(&session("Sugar", 1_760_000_500, 30))
            .await
            .unwrap();

        // A replaced/uninstalled game keeps its past sessions: this is user
        // data, not cache.
        db.replace_store_games(Store::Epic, &[]).await.unwrap();
        assert_eq!(db.launches(10).await.unwrap().len(), 2);

        assert_eq!(
            db.clear_launches(Some(&GameId::new(Store::Epic, "Gizmo")))
                .await
                .unwrap(),
            1
        );
        assert_eq!(db.launches(10).await.unwrap().len(), 1);

        assert_eq!(db.clear_launches(None).await.unwrap(), 1);
        assert!(db.launches(10).await.unwrap().is_empty());
        assert_eq!(db.clear_launches(None).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_corrupt_launch_row_is_reported() {
        let db = Db::open_in_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO launch_history (
                 store, store_id, title, started_at, ended_at, duration_secs, succeeded
             ) VALUES ('epic', 'Gizmo', 'Gizmo', 0, 0, -5, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let error = db.launches(10).await.unwrap_err();
        assert!(
            matches!(error, Error::Data { column, .. } if column == "launch_history.duration_secs"),
            "{error}"
        );
    }
}
