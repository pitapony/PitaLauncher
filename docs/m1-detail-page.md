# M1 — game detail page and Steam playtime

This documents the first slice of the game detail screen and the playtime
source it displays. Hero art, the "Launch" dropdown, and verify/uninstall
actions are deliberately not part of this slice.

## Opening the page

- Click a tile in the library grid. The card itself opens the detail view; the
  Play/Install buttons on the card keep their own behaviour and stop the click
  from bubbling.
- The detail view is keyed by appid, so opening another game remounts its
  profile list.
- The Library / Downloads navigation clears the detail view.

## What it shows

| Field | Source |
|---|---|
| Title, status chip | `ScanReport` / appmanifest `StateFlags` |
| Playtime | Steam `localconfig.vdf`, cached in SQLite `games.playtime_secs` |
| Last played | Steam `localconfig.vdf`, cached in SQLite `games.last_played` |
| Size | appmanifest `SizeOnDisk` |
| Install path | `steamapps/common/<installdir>` |
| Version | appmanifest `buildid` |
| Profiles | SQLite `profiles` table for that `(store, store_id)` |

## Playtime flow

```
<steam>/userdata/<id>/config/localconfig.vdf
  → stores::scan_steam (Playtime is minutes, LastPlayed is Unix seconds)
  → Game.playtime / Game.last_played
  → app::persist::sync
      upsert_games     (metadata only; never touches user data)
      set_playtime     (writes playtime + last_played)
  → Db.games()
  → Library::apply_stored
```

Rules:

- A missing `userdata` directory is normal and silent.
- A malformed `localconfig.vdf` is a scan warning; the rest of the library still
  loads.
- Appids with no manifest are ignored: `localconfig.vdf` remembers games that
  are installed elsewhere.
- `upsert_games` still preserves `playtime_secs`/`last_played` on conflict. The
  scan only writes them through `set_playtime` when it actually found a
  non-zero playtime, so a database-only launch record is not wiped by a scan
  that could not read the VDF.
- No Steam config file is written. The writer remains a separate task with the
  Steam-closed guard, atomic replace, backup and byte-identical restore.

## Profiles

- Profiles are created with `core::Profile::new`, i.e. safe defaults (auto
  Proton, no wrappers, native upscaling, injection off).
- Save / delete go through `app::persist` wrappers over `db::ProfileRepository`.
  Errors are display-ready strings shown inline on the page.
- The profile marked **active** is session state for now. Persisting the choice
  to `~/.local/share/pitagames/profiles/steam-<appid>.json` belongs to the shim
  JSON writer task; the shim read side is still deferred.

## Verification

Pure crates (macOS host):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-stores \
  -p pitagames-shim -p pitagames-cli
CARGO_HTTP_MULTIPLEXING=false cargo clippy \
  -p pitagames-core -p pitagames-db -p pitagames-stores \
  -p pitagames-shim -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

GUI (`crates/app`) in `archlinux:latest`:

```bash
cargo build -p pitagames --all-targets
cargo clippy -p pitagames --all-targets -- -D warnings
cargo test -p pitagames
```

The `wiring_tests` case covers the full path with a real temp Steam tree and a
real SQLite file: scan → sync → profile save/list/delete → write a
`localconfig.vdf` → rescan → the Steam playtime and last-played value win.
