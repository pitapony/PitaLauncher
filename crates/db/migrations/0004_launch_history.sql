-- Launch history.
--
-- One row per supervised game session — a non-Steam launch through the
-- umu/Proton hand-off, the store client, or a native executable. Steam starts
-- its own games (and owns their playtime), so a Steam launch lands in
-- `localconfig.vdf`, not here.
--
-- This is user data, not cache: unlike `games`, rows are never rewritten by a
-- scan, and a session survives the game being uninstalled or the row being
-- replaced. `title` is copied in for the same reason — a history row must still
-- render after the store forgets the game.
--
-- `duration_secs` is the wall clock the hand-off measured (0 when the game never
-- started). `started_at`/`ended_at` are Unix seconds (UTC), so the GUI can show
-- when a session happened and the CLI can sort without parsing text.
--
-- `null` values mean "unknown", not "zero": a failed launch has no exit code,
-- and a launch that never reached `Started` records no mode or log.
--
-- The raw transcript stays in `<data>/pitagames/logs/`; `log_path` only points
-- at it. Nothing from a PTY is copied into the database (an auth run's output
-- can contain a typed code, and a game's log can contain environment secrets).

CREATE TABLE launch_history (
    id            INTEGER PRIMARY KEY,
    store         TEXT    NOT NULL,
    store_id      TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    started_at    INTEGER NOT NULL,
    ended_at      INTEGER NOT NULL,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    mode          TEXT,
    exit_code     INTEGER,
    signal        INTEGER,
    succeeded     INTEGER NOT NULL,
    log_path      TEXT,
    detail        TEXT
) STRICT;

-- The detail page reads one game's most recent sessions.
CREATE INDEX launch_history_game_idx
    ON launch_history (store, store_id, started_at DESC);

-- The History screen reads the newest sessions across every game.
CREATE INDEX launch_history_started_idx
    ON launch_history (started_at DESC);
