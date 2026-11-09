-- Owned-games cache.
--
-- The Steam Web API (`IPlayerService/GetOwnedGames`) answers the whole account
-- library in one call, which is too expensive to repeat on every boot and
-- impossible without the user's API key. This table is that answer, replaced
-- wholesale on each refresh so a game removed from the account disappears.
--
-- `fetched_at` is Unix seconds (UTC) and is the same for every row of one
-- refresh; the refresh policy (currently a day) lives in the app, not in SQL.
-- `playtime_secs` is Steam's `playtime_forever` (minutes) converted to seconds:
-- for an installed game `localconfig.vdf` is more precise and wins in the UI.
--
-- Deliberately no foreign key to games: an owned game is usually not installed,
-- and this cache must survive a scan that finds nothing.

CREATE TABLE owned_games (
    store         TEXT    NOT NULL,
    store_id      TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    playtime_secs INTEGER NOT NULL DEFAULT 0,
    last_played   INTEGER,
    fetched_at    INTEGER NOT NULL,
    PRIMARY KEY (store, store_id)
) STRICT;

CREATE INDEX owned_games_title_idx ON owned_games (title COLLATE NOCASE);
