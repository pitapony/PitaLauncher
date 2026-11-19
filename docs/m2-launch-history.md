# M2 — the launch history (`launch_history`, `app::history_view`, `pitagames-cli history`)

Status: implemented. Verified with fixture databases on macOS and in
`archlinux:latest`, plus an Xvfb run in the container where a seeded Epic row
played a fake game from its grid card and then a row whose executable had been
deleted was played and refused — both sessions landed in SQLite, showed on the
History screen and the detail page's card, and were cleared from the GUI. Not
driven against a real store game.

## Why this slice exists

Playtime was already accumulating: the play worker adds `Outcome::duration` to
the game's row when a session ends (`docs/m2-launch.md`), which is the only
playtime a non-Steam game has. That is a running total, not a record — it cannot
answer "when did this game last run", "did it die on a signal", or "why did Play
do nothing that time". The header strip showed the last verdict and then forgot
it on Dismiss.

The plan's next step was a launch history, so this slice adds one place where a
session is written, one screen that reads all of them, one card that reads a
game's own, and one CLI command.

## What is recorded, and what is not

| Recorded | Why |
|---|---|
| Every **non-Steam** session the GUI starts | It is supervised by `platform::launch`, so the launcher knows when it began and how it ended |
| A launch that **never started** | "No Proton build was found" / "the executable is not on disk" is the row a user needs |
| Exit code, signal, wall-clock duration | The difference between a crash and a clean exit |
| The transcript **path** | Enough to find the log; the log itself stays on disk |
| The title, copied in | A history row must render after the store forgets the game |

| Not recorded | Why |
|---|---|
| Steam sessions | Steam starts its own games and keeps their playtime in `localconfig.vdf`; there is no session for the launcher to own |
| The transcript's contents | A game's log can contain environment values, and an auth run's PTY echo can contain a pasted code. `log_path` is a pointer, never a copy |
| `pitagames-cli launch --run` sessions | It is a diagnostic that prints a verdict instead of owning a session |

## `crates/db` — the table and the repository

`migrations/0004_launch_history.sql`:

```text
launch_history(
  id INTEGER PRIMARY KEY,
  store TEXT, store_id TEXT, title TEXT,
  started_at INTEGER, ended_at INTEGER, duration_secs INTEGER,
  mode TEXT, exit_code INTEGER, signal INTEGER,
  succeeded INTEGER NOT NULL, log_path TEXT, detail TEXT
)
```

Unix seconds, like every other timestamp in the schema. `NULL` means "unknown",
not "zero": a failed launch has no exit code and no mode, and a launch that
never reached `Started` has no transcript. Two indexes, one per read pattern —
`(store, store_id, started_at DESC)` for the detail card, `(started_at DESC)`
for the History screen.

This table is **user data, not cache**. Unlike `games`, nothing rewrites it: a
sync cannot drop a session, and uninstalling the game leaves its history intact
(hence the copied `title`). The only delete is the user's.

`LaunchRecord` is the write shape (`game`, `title`, the two timestamps, a
`Duration`, the mode label, exit code, signal, `succeeded`, `log_path`,
`detail`); `StoredLaunch` adds the row id. `HistoryRepository` has four methods:
`add_launch`, `launches(limit)`, `launches_for(game, limit)` and
`clear_launches(Option<&GameId>)`, which returns how many rows it removed.

**One migration wrinkle was fixed here.** Two processes can open a database with
a pending migration — the GUI and `pitagames-cli`, typically right after an
upgrade. SQLite has no advisory lock, so both read the same pending list, one
wins the `CREATE TABLE` and the other fails with "table launch_history already
exists". `Db::migrate` now retries once on exactly that error
(`is_duplicate_schema`), which re-reads the applied versions and becomes a
no-op. The race was real: it showed up in this container while the GUI and the
tests opened the same file.

## `app::play` — the session becomes a row

`PlayEvent::Started` now carries the `LaunchMode` and `Running` keeps it plus
`started_at`. `Running::record(&outcome, ended_at)` builds the `LaunchRecord`:
on success the outcome's duration/code/signal and its `summary()`
(`exited 0 after 1h 02m`), on failure zero duration and the error text as the
detail. The mode is `None` when the launch failed before a shape was chosen,
which is what lets the History screen say `not started` instead of guessing.

Nothing else in the tick loop changed; the record is a pure function of a
`Running` and an outcome, which is what makes it unit-testable.

## The GUI

`main.rs` drains the play channel as before, but now it reads the `Running`
**before** `apply` frees the slot (the `Finished` event deliberately repeats
neither the start time nor the mode), records the row for a success *and* a
failure, and bumps a `history_revision` signal so both readers reload without a
restart.

- **`app::history_view`** is the new History screen: a nav button next to
  Library/Downloads/Stores/Account/Logs, `HISTORY_LIMIT = 200` rows, one line
  per session (`today 02:14 · Gizmo / epic · gizmo-1 · native · exited 0 after
  3s`), a **Copy log path** button when a transcript exists, a summary line
  (`2 session(s), 1 did not exit cleanly`), Refresh, and **Clear history**.
  Clearing is two clicks on purpose: the first click swaps the button for
  **Confirm clear** / **Keep**. An empty history explains itself instead of
  showing a blank panel.
- **The detail page** gained a `Recent launches` card under About
  (`HISTORY_ROWS = 5`). For a Steam row the card explains that Steam owns its
  sessions rather than showing an empty list, so the absence is a statement, not
  a bug.
- `app::history_view::history_row` is the framework-free mapping from
  `StoredLaunch` to the words on screen, so the formatting is unit-tested
  without a webview.

Rows are read through `persist::recent_launches` / `persist::record_launch_session`
on worker threads; SQLite never runs on the event loop.

## `pitagames-cli history`

```text
pitagames-cli history [<store> <id>] [--limit <n>] [--db <path>] [--json] [--clear]
```

`--limit` defaults to 20 (the GUI asks for 200), `--db` reads a fixture or a
backup instead of the user's database, `--clear` deletes this game's rows or all
of them and prints how many went. A machine that never played a game has no
database; that prints `no launch history yet` (or `[]`) and exits `0` rather
than failing. Text output is a `when / game / mode / result` table with the log
path on a continuation line; `--json` prints the rows verbatim, which is what
the smoke run above was checked against.

The six CLI `launch` tests were made hermetic in the same pass: they now pass
`--db <temp>/absent.db`, so a test run on a developer machine cannot touch the
real library.

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata \
  -p pitagames-platform -p pitagames-stores -p pitagames-secrets \
  -p pitagames-shim -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

In `archlinux:latest` (`CARGO_TARGET_DIR=/target`): `cargo test -p pitagames`,
`cargo test -p pitagames-db -p pitagames-cli`,
`cargo clippy --all-targets -D warnings`, `cargo fmt --all -- --check`.

- `pitagames-db` **27 → 32 tests**: a recorded session round-trips with its
  exit code, mode and log path; a failed launch keeps its `NULL`s and still
  sorts; `launches` is newest-first and respects the limit; `launches_for` is
  scoped to its game; `clear_launches` deletes one game's rows only, and all of
  them when given none.
- `app::play` **7 → 10 tests**: a finished session becomes a history row; a
  launch that never started is recorded with its reason; the mode is absent
  when the shape was never chosen. `crates/app` **118 → 121**.
- `pitagames-cli` **53 → 58 tests**: the parser's defaults and each flag, the
  positional game pair, an unknown store rejected, and the JSON/text shape.
- Xvfb smoke run, fresh `XDG_DATA_HOME` with a seeded `epic/gizmo-1` row whose
  `Gizmo.sh` sleeps 3 s: clicking Play on the card ran it, the header read
  `Gizmo exited 0 after 3s`, the History tab showed one row (`native`,
  `exited 0 after 3s`), the detail page's card showed the same row, SQLite held
  one `launch_history` row with `duration_secs = 3`, `exit_code = 0` and the
  transcript path, and `pitagames-cli history` printed the same in text and
  JSON. A second seeded row whose executable had been deleted recorded
  `not started` with the missing-path message, and the screen's summary read
  `2 session(s), 1 did not exit cleanly`. **Clear history** asked for
  confirmation, then emptied the table (`SELECT count(*) = 0`) and left the
  library rows and playtime untouched.

## Known limits

- **Not driven against a real store game.** The rows come from
  `platform::launch`'s outcome, which the fake-game runs exercised; the umu and
  store-client shapes are the ones `docs/m2-launch.md` documents.
- **A session cut off by closing the window is not recorded.** The worker dies
  with the process, so the row never reaches `Finished`. The playtime has the
  same gap; M4's crash-safety work would need a "session started" row written
  up front.
- **Steam is absent by design**, not by omission. The History screen and the
  detail card both say so.
- **`pitagames-cli launch --run` writes no history.** It is a diagnostic; a CLI
  that owned a session would need the same row write.
- **History grows until cleared.** There is no retention policy or automatic
  pruning; `history --clear` and the GUI's Clear history are the only deletion.
- **`mode` is a label, not a typed enum**, so a future launch shape adds a
  string rather than a migration. The GUI prints whatever it finds.
- **The transcript may be gone** while the row survives — logs are not
  rotated, but a user can delete them. The row still renders; the path is a
  pointer.

## Deliberately not in this slice

- A retention policy, paging in the GUI, or filtering by store/verdict.
- Per-session detail pages (the log viewer is still the Logs tab).
- Writing history from the CLI's launch, or from installs.
- Recording Steam sessions (they belong to Steam) or merging its playtime.
