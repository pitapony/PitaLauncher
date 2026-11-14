# M2 — the library sync (`stores::sync` + `pitagames-cli stores sync`)

Status: implemented. `stores::sync` (the fold and the cache-aware merge),
`pitagames_db::GameRepository::store_games`/`replace_store_games` and
`pitagames-cli stores sync` are verified against fake CLIs and `tempfile`
databases on macOS and in `archlinux:latest`. No real `legendary` / `nile` /
`lgogdownloader` has been driven yet — none is installed on either target
machine. The install queue driver and the GUI side of the wiring are the next
slice.

## Why this slice exists

Each store backend can answer two different questions:

| Store | Owned listing | Installed listing |
|---|---|---|
| Epic | `legendary list --json` | `legendary list-installed --json` |
| Amazon | `nile library list --json` | `nile library list --installed` |
| GOG | `lgogdownloader --list json` | *(none — `gogdl` has no such command)* |

Neither is the library on its own: the owned list has titles for games that were
never downloaded, and the installed list has the path, size and version but only
for what is on this disk. The grid wants both shapes — an installed row to play,
an owned-only row so the game can be installed — and the database is the cache
that makes them available without running three CLIs on every render.

This slice is the write half of the store status wiring: the fold, the merge
policy and the SQLite replacement. The install-queue consumer and the GUI's
install flow come next; doing the data path headless first means the merge rules
are covered by tests before a UI depends on them.

## `stores::sync` — the model

| Item | Role |
|---|---|
| `sync::StoreLibrary` | one store's answer: `installed`, `owned`, `installed_known`, `owned_known`, `warnings` |
| `StoreLibrary::can_replace()` | the owned listing answered (without it there is no library at all) |
| `StoreLibrary::rows()` | the two listings folded into one `Game` per game, sorted by title |
| `sync::merge(cached, fresh)` | the fresh answer reconciled with what the cache already holds |
| `sync::SyncReport` | per-store result for the CLI (`rows`/`installed`/`owned_only`/`removed`/`applied`/`warnings`) |
| `sync::epic_library` / `amazon_library` / `gog_library` | run one backend's listings |
| `sync::Backends` | the three backends, so the CLI/GUI can override programs per store |
| `Backends::all(options)` | every CLI store in `status::CLI_STORES` order |

```text
  Backends::library(store) ──► StoreLibrary { installed, owned, flags }
          │                             │
          ├─ legendary list[-installed] ├─ rows(): installed wins over owned
          ├─ nile text + installed.json └─ merge(cached): cache-aware rows
          └─ lgogdownloader --list json
                                            │
                                            ▼
                          Db::replace_store_games(store, rows)
```

### Folding the two listings

- An **installed row wins** over its owned twin: it carries `install_dir`, `size`
  and `version`, which the owned listing has no idea about. The owned twin only
  fills the fields the install listing never carries — playtime and
  `last_played` — and only when the installed row has no value.
- An owned game with no installed row becomes a **not installed** `Game`, so the
  library can offer Install for it. Owned-only rows deliberately carry no tags;
  the `platform:windows` tag belongs to a real install.
- Rows are sorted by title (case-insensitive) with the id as a tiebreak, so a
  sync and a scan produce the same order.

### The merge policy

The rule that matters is **a listing failure must never delete rows**. Both
legendary and nile log an error and exit `0`, so a failed listing looks exactly
like a valid empty answer unless the caller checks for it.

1. When the **owned listing failed**, nothing is written: the cached rows come
   back unchanged. An empty answer would otherwise delete every row the store
   owns.
2. When the **install listing failed**, an owned row does not count as
   "uninstalled": a cached installed row keeps its `installed` flag, path, size
   and version. A failed listing is not an uninstall.
3. A cached **installed** row the listing never mentions is kept. That covers
   GOG (no install listing at all) and the importers: Heroic/Lutris/Bottles know
   about trees the store CLI cannot report.
4. Everything that remains is sorted again, so the result is deterministic.

`merge` only ever looks at one store's cached rows — a Steam scan and an Epic
sync share the `games` table without either touching the other's rows.

## The database write

`pitagames_db::GameRepository` gained:

- `store_games(store) -> Vec<Game>` — the cached rows a sync merges over.
- `replace_store_games(store, &[Game]) -> LibraryChange { written, removed }` —
  upserts every row and deletes the store's rows the answer no longer names, in
  one transaction.
- The upsert bindings moved to a shared `bind_upsert` helper used by
  `upsert_games` and the new method, so the `ON CONFLICT` behavior cannot drift
  between them. `playtime_secs`/`last_played` stay out of the update set: a store
  listing never knows them, so a resync must not wipe the playtime the store
  reported earlier.

A row whose `GameId.store` is not the store being replaced is refused
(`Error::Data`, `column = "games.store"`) **before** anything is written, so a
caller bug cannot move a Steam row into the Epic swap.

## CLI surface

```bash
pitagames-cli stores sync [--store <name>] [--program <path>] \
    [--game-program <path>] [--log-dir <dir>] [--db <path>] [--dry-run] [--json]
```

- For each selected store it reads the cache, runs the listings, merges and
  replaces that store's rows. `--db` points at another database (a fixture tree,
  a scratch copy); the default is `<XDG_DATA_HOME>/pitagames/pitagames.db`, the
  same file the GUI opens.
- `--dry-run` runs the listings and the merge and prints the plan, but writes
  nothing — the first thing to try on a machine with real accounts.
- A store whose listing failed prints `skipped` with its warnings; the command
  still exits `0`, like `compat` and `import`, because a signed-out store is a
  row state and not a launcher error.
- `--program`/`--game-program` need exactly one `--store`, as with `stores`
  (status), so an override cannot land on the wrong backend.
- `--list`/`--with-stores`/`--steam-root` are refused here, as are `--db` and
  `--dry-run` on `stores` (status) and `stores updates`.

The table prints one line per store:

```text
epic    synced    2 rows · 1 installed · 1 to install  (1 row removed)
amazon  skipped   (the listing was incomplete; the cache is unchanged)
        warning: owned: nile exited 1
```

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata \
  -p pitagames-platform -p pitagames-stores -p pitagames-secrets \
  -p pitagames-shim -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

- `stores::sync` (15 tests): the fold prefers installed rows and sorts by title;
  an owned twin only fills what the install listing lacks and never overwrites a
  known playtime; a failed install listing keeps a cached install (path and
  size intact); an answered install listing does bring an uninstall back; an
  installed row a listing never mentions survives (GOG/importers); an incomplete
  answer returns the cache untouched; a new owned game is added and a dropped
  owned-only row is gone; a Steam row is neither kept nor allowed to block an
  Epic row; the Epic/Amazon/GOG listings run through the real parsers against the
  fixture CLIs (including the signed-out exit-0 case); all three stores run in
  `CLI_STORES` order and fail together quietly; a store with no backend is
  refused rather than guessed.
- `pitagames-db` (5 new): replacing a store's rows writes and drops in one call;
  another store is never touched; a replaced row keeps the stored playtime;
  an empty answer clears that store only (and a second empty replace is a
  no-op); a foreign-store row is refused before any write.
- `pitagames-cli` (3 new, plus the parser and refusal tests extended): the parser
  (`sync`, `--db=`, `--dry-run`) and the cross-subcommand refusals; an end-to-end
  `stores sync --store epic --program …` over a fake `legendary` into a
  `tempfile` database (the Epic rows land, the DLC is filtered, an owned-only row
  the store dropped is removed, the Steam row is kept); `--dry-run` writes
  nothing; a legendary that logs an error and exits `0` leaves the seeded row
  alone.
- `crates/app` still builds, lints and tests in `archlinux:latest` (it shares
  `pitagames-db`), 73 tests.

## Known limits

- **Not yet run against a real store CLI.** The listing shapes come from the
  fixtures the backends already use; the first machine with `legendary`/`nile`/
  `lgogdownloader` installed is the closing check.
- **The GUI does not read these rows yet.** The library grid is still Steam-only
  (`GameEntry`); showing Epic/Amazon/GOG rows is the GUI half of the wiring.
- **The install queue is not driven.** `stores::queue` still has no consumer, and
  the install button for a CLI-store row is part of the next slice.
- **GOG can never report an uninstall.** With no install listing, a GOG row only
  loses `installed` when the row itself disappears from the owned listing.
- **No metadata or artwork for these rows.** The Steam `appdetails` fetch is
  keyed by appid; CLI-store titles have no art until a later slice decides where
  it comes from.
- **Playtime lands in `games`, not `owned_games`.** The CLI stores' playtime is
  written with the game row and preserved across syncs; the `owned_games` cache
  stays the Steam Web API's table.

## Deliberately not in this slice

- The install queue's driver and the GUI's install flow (the next slice).
- An in-app PTY terminal for auth (M2.5).
- Artwork or metadata for non-Steam games.
- A scheduled/background sync — the CLI (and, next, the GUI) calls it
  explicitly.
