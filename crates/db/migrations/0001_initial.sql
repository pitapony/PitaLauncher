-- PitaGames initial schema.
--
-- Paths and timestamps follow the core domain types: timestamps are Unix
-- seconds (UTC), paths are TEXT because PathBuf's serde representation is a
-- string, and tags are a JSON array.

CREATE TABLE games (
    store         TEXT    NOT NULL,
    store_id      TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    install_dir   TEXT,
    exe           TEXT,
    installed     INTEGER NOT NULL DEFAULT 0,
    size_bytes    INTEGER,
    version       TEXT,
    last_updated  INTEGER,
    playtime_secs INTEGER NOT NULL DEFAULT 0,
    last_played   INTEGER,
    anticheat     TEXT    NOT NULL DEFAULT 'unknown',
    tags          TEXT    NOT NULL DEFAULT '[]',
    -- Internal bookkeeping: when this row was last confirmed by a scan.
    last_seen     INTEGER NOT NULL,
    PRIMARY KEY (store, store_id)
) STRICT;

CREATE INDEX games_title_idx ON games (title COLLATE NOCASE);
CREATE INDEX games_installed_idx ON games (installed);

-- Named launch profiles. The profile payload is the serde JSON form of
-- core::profile::Profile, so schema changes to the profile do not need a
-- migration as long as serde defaults stay backward compatible.
CREATE TABLE profiles (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    store        TEXT    NOT NULL,
    store_id     TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    profile_json TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE (store, store_id, name)
) STRICT;

CREATE INDEX profiles_game_idx ON profiles (store, store_id);
