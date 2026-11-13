# M2 — the Heroic, Lutris and Bottles importers (`stores::import`)

Status: implemented and verified against fixture trees locally (macOS) and in
`archlinux:latest`. **Not yet run against a real Heroic, Lutris or Bottles
install**: none of the three is installed on either target machine. Every path
and field quoted below was taken from their sources (`src/backend/constants/
paths.ts`, `src/backend/storeManagers/*/library.ts`, `src/common/types.ts`,
`src/common/types/legendary.ts`, `src/backend/cache.ts` for Heroic;
`lutris/game.py`, `lutris/settings.py`, `lutris/database/games.py` for Lutris;
`bottles/backend/globals.py`, `bottles/backend/managers/library.py` for
Bottles), not from a live run.

## Why an importer at all

Three of the four stores PitaGames covers are driven by a CLI
(`legendary`, `nile`, `lgogdownloader`/`gogdl`); see `docs/m2-epic.md`,
`docs/m2-amazon.md` and `docs/m2-gog.md`. A user who has none of them installed
gets an empty library from those backends, even though the games are already on
disk — because they were installed by **Heroic**, **Lutris** or **Bottles**.

The plan's answer is an **installed-only, read-only import**: read what those
launchers already recorded and show it, with "install `<cli>` to enable
installs" as the only action offered. Nothing is launched, downloaded or
written, and no store session is needed: this is also the path for a user who
does not want to hand a second launcher their Epic/Amazon credentials.

```text
  Importer::default_roots() / with_roots(heroic, lutris, bottles) ──► scan()
      │
      ├─ heroic  <config>/heroic/legendaryConfig/legendary/installed.json  JSON   → Store::Epic
      │          <config>/heroic/gog_store/installed.json                  JSON   → Store::Gog
      │          <config>/heroic/nile_config/nile/installed.json           JSON   → Store::Amazon
      │          <config>/heroic/sideload_apps/library.json                JSON   → Store::Local
      │          <config>/heroic/store_cache/gog_library.json              (titles only)
      │          <config>/heroic/nile_config/nile/library.json             (titles only)
      ├─ lutris  <data>/lutris/pga.db                                      SQLite → Store::Local
      └─ bottles <data>/bottles/library.yml                                YAML   → Store::Local
```

## Shape

| Item | Role |
|---|---|
| `import::Origin` | `Heroic` / `Lutris` / `Bottles` — the launcher a record came from |
| `import::Importer` | the three root lists: `default_roots()`, `with_roots()`, `heroic_roots()`, `lutris_roots()`, `bottles_roots()`, `scan()` |
| `import::ImportReport` | `games`, `sources` (one per file), `warnings` (title-only files), `duplicates`; `games_from(origin)`, `readable()`, `failed()` |
| `import::SourceReport` | `origin`, `kind` (`legendary`, `gog`, `amazon`, `sideload`, `pga.db`, `library.yml`), `path`, `games`, `error`, `found()` |
| `import::merge(library, imported)` | adds the imported games a library does not already have, by `GameId` |
| `import::imported_tag(origin)` | the `imported:<launcher>` tag every imported game carries |

`Importer::default_roots()` is `Importer::default()`, so a caller that wants the
usual locations does not need to name them. All three lists are **directories**
the caller passes in, exactly like `platform::proton`'s roots and
`platform::compat`'s directories, so a fixture tree can be scanned with no real
install anywhere.

## What the plan got wrong: Lutris is a database

The plan listed `~/.local/share/lutris/games/*.yml` as the Lutris source. That
is half true and the missing half matters: Lutris 0.5 keeps the **game list** in
SQLite (`<data>/lutris/pga.db`, `games` table) and the `*.yml` files under
`<config>/lutris/games/` only hold the **runner configuration** (`game.exe`,
`game.prefix`, …). A title, the install directory and the installed flag are
only in the database, so an import that read the YAML would have had to invent
titles from file names.

So this importer reads `pga.db`, which is the first time PitaGames opens
another application's database. The rules that keeps honest:

- **Read-only.** The open is `SQLITE_OPEN_READ_ONLY`; a refused read-only open
  (a WAL database needs its `-shm` file, and Lutris is often running) falls back
  to a read-write open **without `SQLITE_OPEN_CREATE`**, so a database that is
  not there stays absent. The only statement is `SELECT * FROM games`.
- **`SELECT *` and read by name.** The `games` table is Lutris' schema, not
  ours: `name`, `slug`, `runner`, `platform`, `directory`, `installed`,
  `configpath` and `service` are read when they are there, and a Lutris release
  that renames, adds or drops a column costs that one field. A test builds a
  table with a single `name` column and every row still imports.
- **Values are read as SQLite values**, not as strings: `installed` is an
  integer and `name` is text, and asking rusqlite for one type would fail on the
  other.

The identity is the **slug** (`hollow-knight`), because that is what
`lutris <slug>` uses and what survives a reinstall; the numeric row id changes.

## Heroic: one root, four JSON files

Heroic's config folder is `~/.config/heroic` (or the Flatpak copy below), and
each store manager writes its own install records under it. All of them are
JSON, so the import is a set of tolerant `serde_json` reads with
`#[serde(default)]` on every field.

| File | Shape | Games become |
|---|---|---|
| `legendaryConfig/legendary/installed.json` | an array of `InstalledJsonMetadata` | `Store::Epic`, id = `app_name` |
| `gog_store/installed.json` | `{"installed": [ … ]}` | `Store::Gog`, id = `appName` |
| `nile_config/nile/installed.json` | nile's own array (`id`, `path`, `version`, `size`) | `Store::Amazon`, id = the product id |
| `sideload_apps/library.json` | `{"games": [ … ]}` of `GameInfo` | `Store::Local`, id = `app_name` |

- **The ids are the stores' own ids**, not something derived here: a Heroic
  Epic `app_name` is the same string `legendary list --json` reports, so
  `merge` recognises a game the Epic backend already has. That is the whole
  point of importing instead of creating a local row per game.
- **DLC is dropped** (`is_dlc` on the legendary, GOG and sideload records), the
  same call `stores::epic` makes, so an add-on does not become a second tile.
- **Titles come from two places.** legendary's own `installed.json` has `title`.
  GOG's does not, so `store_cache/gog_library.json` (`{"games": […]}`) supplies
  it; nile's does not either, so `nile_config/nile/library.json` does — and that
  file is nile's cache, which `stores::amazon::parse_owned` already parses for
  `pitagames-cli amazon list`, so the two backends cannot drift. A missing or
  broken title cache costs the **titles only**: the row falls back to the store
  id and the source still imports.
- **Amazon is nile's file.** Heroic copies the global `~/.config/nile` into
  `heroic/nile_config/nile`, so `stores::amazon::installed_state` reads it
  unchanged — the same function the Amazon backend uses.
- **The executable is resolved when it is relative.** legendary writes
  `executable` relative to `install_path`
  (`FortniteGame/Binaries/Win64/FortniteLauncher.exe`), so it is joined; Heroic's
  GOG records write an absolute **Windows** path (`C:\Games\…`), which is left
  as it is: turning it into a host path needs the Wine prefix, and that is the
  prefixes slice.

## Bottles: `library.yml`

Bottles' `<data>/bottles/library.yml` is a YAML **map** of uuid to entry, and it
is the file that holds what the per-bottle `bottle.yml` does not: a display
name.

```yaml
c2b3f4a4-…:
  id: 6e1a3f5c-…            # the entry id, which is the game's id here
  name: Notepad++
  source: bottle             # `bottle` (a Wine program) or `umu`
  bottle: { name: Gaming, path: /home/player/.local/share/bottles/bottles/Gaming }
  thumbnail: 9f8e7d6c-…
d3c4e5f6-…:
  id: 7f2b4c6d-…
  name: Hades
  source: umu                # started through umu, so it belongs to another store
  source_id: "1145360"
  store: steam
```

- The game id is the entry's `id`, falling back to the map key.
- **No install directory is invented.** A `bottle` entry's program path is a
  Windows path inside the prefix, and a `umu` entry is another store's game:
  neither is a directory this launcher owns. The bottle name, the source and the
  store are recorded as tags instead, and the paths stay in the other launcher's
  files until the prefixes slice needs them.
- The YAML is read with `serde_norway` (the maintained `serde_yaml` fork), not a
  hand-written parser: a map of uuid to mapping, with `null` entries tolerated.

## Flatpak

All three have a Flatpak build, and a user can have the native one, the Flatpak
one, or both. The Flatpak copies keep their XDG directories under
`~/.var/app/<id>/`, so the default roots are:

| Launcher | Native | Flatpak |
|---|---|---|
| Heroic | `$XDG_CONFIG_HOME/heroic` | `~/.var/app/com.heroicgameslauncher.hgl/config/heroic` |
| Lutris | `$XDG_DATA_HOME/lutris` (`pga.db`) | `~/.var/app/net.lutris.Lutris/data/lutris` |
| Bottles | `$XDG_DATA_HOME/bottles` (`library.yml`) | `~/.var/app/com.usebottles.bottles/data/bottles` |

Bottles' own runtime data directory is `/var/data` inside the sandbox (its
`Paths.base` asks for exactly that), which is the Flatpak row above on the host.

## Tags

Imported games carry their provenance as tags, which is the only field the
`Game` model has room for and what a store/detail row will read:

| Tag | Meaning |
|---|---|
| `imported:heroic` / `imported:lutris` / `imported:bottles` | which launcher the row came from |
| `platform:windows` / `platform:linux` / `platform:mac` | the record's own platform, lowercased |
| `runner:wine`, `runner:libretro`, … | Lutris' runner for the game |
| `service:steam`, … | the service a Lutris row belongs to |
| `bottle:<name>` | the Bottles bottle a program lives in |
| `source:bottle` / `source:umu`, `store:steam` | a Bottles entry's source, and its store when it has one |

`platform:windows` is the same tag Epic's and Amazon's parsers emit, and the
input the runtime/graphics UI uses to decide whether Proton is needed. Nothing
is guessed: a record with no platform gets no platform tag.

## Failure model

An import must never take the library down with it, so failure is per source:

- A **missing** file is not an error — most users have none of these launchers.
  It is reported as `(missing)`, which is how the store UI can say "Heroic is not
  installed" rather than showing an empty list.
- A file that exists and **does not parse** is an error for that source alone.
  The other sources still import, the CLI prints the reason to stderr, and the
  exit code stays `0` (like `pitagames-cli compat`'s warnings): the user asked
  what is on disk, and something on disk is broken.
- A **title-only** cache failing is a warning, not a source error: it costs
  titles, never rows.
- **Duplicates** are dropped by `GameId` (`ImportReport::duplicates`). Two Heroic
  roots pointing at one config, or a sideload entry for a game Heroic also owns,
  must not produce two tiles.

## Merging into a library

```rust
let imported = pitagames_stores::import::Importer::default().scan();
let added = pitagames_stores::import::merge(&mut library, imported.games);
```

A game the library already has **wins**: a real scan knows the installed state,
Steam's playtime and the update flags, which another launcher's record does not.
`merge` is the whole bridge the GUI needs, and it is deliberately not called from
inside `scan` — the importer does not know what the caller already has.

## CLI surface

```bash
pitagames-cli import [--heroic <dir>]... [--lutris <dir>]... [--bottles <dir>]... [--json]
```

Each flag is repeatable and replaces the default for **that** launcher only, so
`import --heroic ./fixtures/heroic` still looks for a real Lutris and Bottles.
`--json` prints the whole `ImportReport` (games included), which is what a bug
report needs. The listing is one line per file:

```text
Heroic   legendary   /home/player/.config/heroic/legendaryConfig/legendary/installed.json  (2 games)
Heroic   gog         /home/player/.config/heroic/gog_store/installed.json  (1 game)
Lutris   pga.db      /home/player/.local/share/lutris/pga.db  (2 games)
Bottles  library.yml /home/player/.local/share/bottles/library.yml  (missing)

11 games: gog 1, epic 2, amazon 2, local 6
```

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-stores -p pitagames-cli
cargo clippy -p pitagames-stores -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`import::tests` (16) covers: legendary records (DLC dropped, relative executable
joined, size/version/tags) and a non-JSON document as one source error; the GOG
records with titles from the cached library, a string install size, and the
id-as-title fallback; Amazon through nile's own records; the sideload entries
(including the uninstalled URL shortcut); **one broken Heroic file not stopping
the others** (`failed()` has exactly `legendary`, the broken title cache is a
warning, and the GOG row survives); a Lutris database built with the real column
set and one with a single `name` column (both import, the slug is the identity,
`installed` is read from the integer); a missing versus unreadable `pga.db`; the
Bottles map (entry id, bottle/source/store tags, no invented install directory);
a missing and a broken `library.yml`; a scan over two identical roots that
deduplicates to one row per id; a scan of nothing installed that is empty,
silent and error-free; `merge` keeping the library's own copy; and the default
roots following `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/`HOME` plus all three Flatpak
locations. `cli` gained a `import` parser test and a test that renders the
source lines and the store summary from a scanned fixture tree.

The fixtures are `fixtures/heroic/**` (legendary's `installed.json` with a DLC
entry and a Mac title, the GOG store plus its title cache, the sideload library)
and `fixtures/bottles/library.yml`; the Amazon side reuses `fixtures/nile/`
directly, and the Lutris database is **built in the test** with `rusqlite`, so
no binary blob is checked in and the schema each test relies on is visible in the
test. Manually, `pitagames-cli import` was run against a fixture tree with a real
generated `pga.db`: the listing above, `--json` with the games, and a run with a
broken GOG cache and a non-database `pga.db` (two warnings on stderr, both
surviving sources, exit `0`).

## Known limits

- **Not yet run against a real Heroic, Lutris or Bottles.** The paths and shapes
  come from their sources; the first real-machine run (import a Heroic Epic
  game, a Lutris game and a Bottles program, then check `merge` against a Steam
  scan) is the closing check, like the M0 hardware closer.
- **Installed-only.** Nothing is offered for a game Heroic owns but has not
  downloaded; the owned-games side stays with the store backends.
- **No artwork.** Heroic's caches carry `art_cover`/`art_square` URLs and Lutris
  has cover art under `<data>/lutris/coverart`, but the art pipeline only speaks
  Steam today.
- **No launching.** An imported game has no launch path yet: that is the
  umu/Proton prefixes item, which is also where Lutris' `prefix`/`exe` and
  Bottles' bottle paths get read.
- **No playtime or last-played for imports.** Steam's numbers come from
  `localconfig.vdf`; Lutris' `playtime`/`lastplayed` columns are not read because
  their unit has changed between Lutris releases and a wrong playtime is worse
  than none.
- **One Lutris database per root.** Lutris' `pga_path` setting can move
  `pga.db`; that would need reading `lutris.conf`, which this slice does not.
- **Bottles v1 (`~/.Bottles`) is not read** — v2's `library.yml` is.
- **The importer is not wired into the GUI yet.** `scan`/`merge` are headless on
  purpose; the store status row and the "these came from Heroic" copy are the
  next M2 item.
- **Two SQLite bindings, one build.** `pitagames-stores` now links `rusqlite`
  for Lutris' database while `pitagames-db` links `sqlx`. They resolve to the
  same `libsqlite3-sys` 0.30, so an Arch build compiles SQLite once.

## Deliberately not in this slice

- The store status UI, the DB rows and the owned-games cache.
- Prefixed launches: umu/Proton, the shim profile, the wrapper chain.
- Art sourcing, update aggregation, uninstall.
- Steam's own `libraryfolders.vdf` (that is `stores::steam`, and a flatpak Steam
  root is a scanner question, not an importer one).
- Importing *into* another launcher, or writing anything back at all.
