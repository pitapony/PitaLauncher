# M2 — store installs from the GUI (`stores::installer`, `app::store_installs`)

Status: implemented. `stores::installer` is verified against fake CLIs on macOS
and in `archlinux:latest`; the GUI is built, linted, tested and smoke-run under
Xvfb in the container, where a fake `legendary` installed a game from a grid
card. No real `legendary` / `nile` / `gogdl` has been driven yet — none is
installed on either target machine.

## Why this slice exists

The previous three slices produced everything except the part that does the
work: `stores::status` reports whether a store is usable, `stores::sync` writes
the store's rows into SQLite, and `stores::queue` holds the serial install
queue — with **no consumer**. The grid was still Steam-only, and the only way to
install an Epic/Amazon/GOG game was `pitagames-cli <store> install`.

This slice closes that loop:

```text
 grid card  ──Install──►  installer::plan ──► InstallJob ──enqueue──► InstallQueue
                                                                            │
 Stores screen ──Sync libraries──► stores::sync ──► SQLite ──► grid rows     │ start_next
                                                                            ▼
                     InstallEvent ◄──progress── installer::run (worker thread)
                          │
             Downloads board ──► grid row flips to installed ──► SQLite upsert
```

## `stores::installer` — what a job is, and how it runs

Three modules already owned a piece of a non-Steam install, and none of them
knew about the others: `queue` owns the ordering and the disk pre-check, `sync`
owns what an account owns, and the backends (`epic`, `amazon`, `gog`) own the
argv. `installer` is the seam.

| Item | Role |
|---|---|
| `installer::InstallPlan` | `{ id, title, program, target }` — where the game lands and which CLI fills it |
| `InstallPlan::job()` | the queue entry (`InstallJob`), carrying the target |
| `InstallPlan::command()` | the `CliCommand::Install { id, install_dir }` a backend needs |
| `installer::plan(id, title)` | the pure half: resolve the directory, no I/O |
| `installer::run(backends, job, options, on_progress)` | the blocking half: drive the CLI, report progress, return the verdict |
| `installer::InstallRun` | `{ program, target, transcript }` on success |
| `installer::InstallerError` | `Unsupported(store)`, `NoDataDir` |

**The target is the store's own directory.** `plan` calls `epic::install_dir`,
`amazon::install_dir` or `gog::install_dir`, the same functions
`pitagames-cli epic|amazon|gog install` uses, so the GUI and the CLI cannot
drift. A hostile title still cannot escape it: `paths::safe_component` sanitises
the component (`A/B` → `A-B`). The plan is pure, which is why the queue row can
show its directory before any process exists.

**The job, not the title, is the input to a run.** `run` reads `job.id.store`
and `job.target`; it never re-derives a path. A retried job therefore fills the
directory the user was shown, even if the store's title changed in between.

**One verdict rule for three CLIs.** `RunReport::succeeded()` — exit `0` *and*
no `ERROR`/`CRITICAL`/`FATAL` line — is the install verdict for every backend.
legendary and nile log an error and exit `0` for some refusals; `gogdl` exits
non-zero. The CLI's own summary line is not trusted, and `launch`'s
`exited_cleanly()` rule does not apply here (the game is not sharing the PTY).

**Progress is deduplicated in the driver, not the UI.** `ProgressTracker` folds
`ProgressEvent`s into the `JobProgress { percent, stage }` shape the queue
takes: a blank line or a repeated stage is not news, `Done`/`Failed` are ignored
(the verdict is `RunReport`), and a stage line is capped at 200 characters plus
an ellipsis so one chatty line cannot blow up a Downloads row.

A failure comes back display-ready: `"<store> install failed: <the CLI's own
message> (transcript: <path>)"`, or `"could not run the <store> install: …"`
when the program is not on `PATH` (the error names the CLI).

## `app::store_installs` — the board between the queue and the window

Framework-free like the rest of `library`/`downloads`, so it is unit-tested
without Dioxus.

| Item | Role |
|---|---|
| `InstallEvent` | `Progress { index, progress }`, `Finished { index, result }` — one report from a worker |
| `Installs` | the `InstallQueue` plus the accessors the two views need |
| `Installs::active_label(id)` | `"queued"`/`"installing"` for a card, `None` once finished |
| `Installs::apply(event)` | fold a worker report in (a late report for a cancelled job is refused by the queue) |
| `Installs::cancel/retry/remove/clear_finished` | the Downloads row buttons |
| `store_installs::spawn(job, index, sender)` | one worker thread per running job |
| `InstallChannel` | the app's `sender()` + `async next()` pair |

The worker thread owns **only the job**: the queue stays on the UI side, so a
window that closes mid-install cannot corrupt it, and the thread reports through
the same unbounded channel the rest of the app uses. Its name is
`pitagames-install-<store>` so a hang is identifiable in `top`.

`InstallChannel` exists because Dioxus hooks need a `Clone` value and an
`UnboundedReceiver` is not one: the receiver sits behind an `Rc<AsyncMutex<..>>`
exactly like `activation::Requests`. The app owns one channel; the grid holds
the sender, one drain future owns the receiver.

## The GUI side

**Startup.** After the Steam scan and the owned-games cache,
`persist::load_store_rows` reads every CLI store's rows out of SQLite and
`Library::apply_store_rows` folds them in. The rows are never scanned, so a
store whose CLI is missing (or signed out) keeps whatever the last sync wrote —
a failed listing is not an uninstall, at the data layer and again here.

**The grid card** now branches on the store:

| Card | Action slot |
|---|---|
| Steam, installed | **Play** (`steam://run/<appid>`), disabled while busy |
| Steam, not installed / update | **Install**/**Update** → `steam://install/<appid>`, then the manifest poller |
| CLI store, not installed | **Install** → `installer::plan` → enqueue → `pump_installs` → Downloads |
| CLI store, queued/running | `queued` / `installing` label — the label replaces the button, so a second click cannot queue a duplicate |
| CLI store, installed | `installed` label (Play arrives with the umu/Proton hand-off) |

`GameEntry::play_is_wired()` is the one place that answers "can this row
launch", which is why the grid and the detail page agree.

**Finishing an install** flips the row in memory (`Library::apply_install`)
*and* writes it (`persist::record_install` → `upsert_game`), so the next start
shows the game installed without waiting for a sync. The queue then gets the
next job (`pump_installs`), which is what keeps the queue serial: the disk
pre-check runs there (`platform::disk::space`), a job refused for space is
already `Failed` with the numbers, and the pump keeps looking for one it can
start.

**The Downloads board** gained a "Store installs" section next to the Steam
rows: title, store and id, state word, percent bar, the CLI's last stage line
and the target directory — plus Cancel (queued only), Retry, Remove and "Clear
finished". The nav count adds the store queue's active jobs to Steam's.

**The Stores screen** gained **Sync libraries**: the three CLIs run on a worker
thread (`store_sync::fetch`), the answer comes back over a channel, and a drain
future merges it over the cache and writes it (`store_sync::apply_all` →
`stores::sync::merge` + `replace_store_games`). The screen reports
`"N game(s) from M of 3 store(s)"` plus up to three per-store warnings, and the
grid picks the rows up through `apply_store_rows` without a restart.

**The detail page** hides what cannot work: Play only when
`play_is_wired()`, and the Profiles/Runtime panels only for Steam, with a note
that per-game runtime settings arrive with the umu/Proton hand-off. The About
block no longer claims Steam will describe a GOG game.

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata \
  -p pitagames-platform -p pitagames-stores -p pitagames-secrets \
  -p pitagames-shim -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

In `archlinux:latest` (`CARGO_TARGET_DIR=/target`):

```bash
cargo test -p pitagames
cargo clippy -p pitagames --all-targets
```

- `stores::installer` (11 tests): a plan names each store's own directory (and a
  hostile title cannot escape it); a plan becomes the queue entry and its
  `Install` command; Steam/Local are refused; a missing data directory is named,
  not guessed; a fake `legendary` reporting `42.5%` → `100%` succeeds and the
  progress lands in order; an install that logs an error and exits `0` fails; a
  backend that exits non-zero fails; a missing program is reported with the store
  and CLI name; a Steam job is refused by the runner too; the tracker ignores
  blanks and repeats; a long stage line is capped.
- `stores` total 244 tests (+11).
- `crates/app` 88 tests (+15): `store_installs` (7 — the job walks
  queued → running → done, a duplicate is refused while active, progress lands on
  the running row and stops after it finishes, a failed job retries while a
  finished one is removed, a short-on-space job is skipped without starting, a
  retry keeps the target, the grid label tracks only an active job),
  `store_sync` (4 — one answer per requested store, the rows land in the cache, a
  failed listing leaves the cache alone, one broken store does not stop the
  others) and `library` (4 — cached store rows join the grid and leave Steam
  alone, a re-sync replaces rows in place, a store row names its store and its own
  id, a finished install flips its row).
- Xvfb smoke run, with a fake `legendary` on `PATH` and one seeded Epic library:
  the grid showed the installed Epic row with an `installed` label and the owned
  row with an **Install** button; clicking Install jumped to Downloads, the job
  ran and reported `installed`, the grid card flipped to `installed`, and the row
  was in SQLite (`installed = 1`, `install_dir` set). The Stores screen's **Sync
  libraries** reported `2 game(s) from 1 of 3 store(s). 3 warning(s).` with one
  line per missing CLI. A seeded Steam manifest still produced the Play button,
  the Profiles and Runtime panels and the detail hero.

## Known limits

- **Not run against a real store CLI.** The argv and the log shapes come from
  the fixtures the backends already use; the first machine with a real account
  is the closing check.
- **A running install cannot be cancelled from the UI.** `Installs::cancel` only
  cancels `Queued` jobs: the driver supports cancellation, but the UI has no
  channel into the worker that owns the handle yet. The row says what happened;
  the process finishes on its own.
- **No size on a job, so no real disk pre-check.** No store reports a download
  size in its listing, so `InstallJob::size` is `None` and `start_next` skips the
  check (an unknown answer is not a refusal). The plumbing is there for the store
  that starts reporting one.
- **Everything in the queue is lost on exit.** The queue lives in the window;
  a job that was running when the app closed has no resume. The store CLI may
  leave a partial download its own `verify`/repair can fix.
- **A fresh install can lose to an answered install listing.** If a store's
  install listing answered and does not mention a game we just recorded as
  installed (a store CLI with a stale cache), the next sync believes the listing
  — the store is the authority for its own installs.
- **No artwork or metadata for store rows.** The Steam `appdetails` fetch is
  keyed by appid, and `metadata_candidates` skips non-numeric ids, so these cards
  show a letter placeholder until a later slice decides where store art comes
  from.
- **Play is still Steam-only**, and so are Profiles/Runtime.

## Deliberately not in this slice

- The umu/Proton launch hand-off that turns a prefix into a running game (the
  next slice; `platform::prefix` + `platform::envc` already have the pieces).
- An in-app PTY terminal for auth (M2.5).
- A background or scheduled sync — the Stores screen calls it explicitly.
- Download resumption across restarts.
