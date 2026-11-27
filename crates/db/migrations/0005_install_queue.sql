-- Persisted install queue.
--
-- The CLI-store install queue (`stores::queue`) lives in memory while the
-- launcher runs, but a launcher crash must not lose it: an install that was
-- `running` left a store CLI orphaned, and a row that was `queued` still owes
-- the user a download. The whole queue is replaced on every state change
-- (one transaction), so this table is a snapshot, not a log.
--
-- `pid` is the running store CLI's process-group id, recorded moments after
-- the process starts. At the next boot the launcher turns every `running` row
-- into a failure and reaps the orphan by that pid (identity-checked against
-- `/proc`; see `stores::recovery`). A row with no pid never reached the spawn.
--
-- `kind` and `state` are the queue's own stable wire tokens ("install",
-- "update", "verify"; "queued", "running", "done", "failed", "cancelled"). A
-- token this build does not know is skipped on load, never guessed.
--
-- The `message` column holds a failed job's reason; `percent`/`stage` hold the
-- progress the row had reached, so a restored row renders the bar it had.
--
-- Nothing here deletes a partial download: the store's own verify/repair is
-- what reconciles an interrupted install (the row's Verify action), so no
-- filesystem path beyond the target directory is persisted or touched.

CREATE TABLE install_queue (
    id         INTEGER PRIMARY KEY,
    position   INTEGER NOT NULL,
    store      TEXT    NOT NULL,
    store_id   TEXT    NOT NULL,
    title      TEXT    NOT NULL,
    target     TEXT    NOT NULL,
    size_bytes INTEGER,
    kind       TEXT    NOT NULL,
    state      TEXT    NOT NULL,
    message    TEXT,
    pid        INTEGER,
    percent    REAL,
    stage      TEXT
) STRICT;

-- The queue is read whole, youngest first only when rendering; the read order
-- is the insertion order the launcher assigned (`position`).
CREATE INDEX install_queue_position_idx ON install_queue (position);
