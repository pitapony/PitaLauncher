-- Store metadata cache.
--
-- One row per appid, including apps the store refused to describe: a NULL
-- payload is a negative cache entry ("no data"), so a delisted app is not
-- retried on every launch. `fetched_at` is Unix seconds (UTC); the freshness
-- policy lives in pitagames-metadata, not in SQL.
--
-- Deliberately no foreign key to games: metadata is a cache that may outlive a
-- scan (an uninstalled game keeps its art and description).

CREATE TABLE game_metadata (
    store        TEXT    NOT NULL,
    store_id     TEXT    NOT NULL,
    payload_json TEXT,
    fetched_at   INTEGER NOT NULL,
    PRIMARY KEY (store, store_id)
) STRICT;

CREATE INDEX game_metadata_fetched_idx ON game_metadata (fetched_at);
