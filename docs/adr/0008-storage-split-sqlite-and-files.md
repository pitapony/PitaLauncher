# ADR-0008: Storage split — SQLite for data, files for shipped/hand-edited config

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §8 (Storage); `AGENTS.md` (compat DB is files; art is served)

## Context

PitaGames persists several different kinds of thing: a scanned library, a
metadata cache, per-game launch profiles, a shipped-and-user-editable
compatibility database, user preferences and input maps, and a log directory.
One store would be simpler to reason about but wrong for the items that must be
shipped with the package, merged field-by-field, or hand-edited by a user.

## Decision

Split by shape:

- **SQLite** (via `sqlx`, runtime queries, `migrate!`) for the library
  (`games`), the owned-games cache, the `game_metadata` cache and the
  `launch_history` user-data table. Anything that needs queries, joins or
  transactional updates goes here.
- **Shipped/hand-edited files** for everything a human or the package owns:
  - `compat/*.toml` (shipped) + `<config>/pitagames/compat/*.toml` (user
    overrides), merged field-by-field — *not* SQLite.
  - `<config>/pitagames/config.toml` — UI/shell/display preferences.
  - `<config>/pitagames/input.toml` — per-device input maps.
  - `<data>/pitagames/profiles/steam-<appid>.json` — the shim's per-game profile
    (atomic writes; see ADR-0017).
- **Art** in `<cache>/pitagames/art/` as 2× WebP thumbnails, served over a
  token-guarded loopback server (never `file://`, never base64-inlined) and only
  rendered once the file exists on disk.
- **Logs** and **backups** are files under `<data>/pitagames/`; a log's *path*,
  never its contents, may enter SQLite.
- A missing config file is the defaults; a malformed one is a warning plus the
  defaults. A single-instance `flock` on `<data>/pitagames.lock` protects Steam
  config from two windows.

## Consequences

- Positive: the compat DB can ship in the package and be merged/validated as
  typed data; profiles and input maps are diffable and recoverable.
- Positive: `sqlx` migrations only own the data that genuinely needs a database.
- Negative: two persistence mechanisms to keep in mind, and a rule to apply per
  new piece of state (data vs. shipped/hand-edited config).
- Secrets never enter either store (see ADR-0009).

## Alternatives considered

- **Everything in SQLite (including the compat DB).** Rejected — the compat DB is
  shipped, overridden per field, and read by the CLI without a database.
- **Everything in files.** Rejected — the library and history want queries and
  transactions.

## References

- plan §8 *Storage*.
- `AGENTS.md` — *Art is served, not inlined*; *The compat DB is files, not
  SQLite*; *Secrets never leak*.
- `docs/m1-compat-db.md`, `docs/m1-art.md`, `docs/m2-launch-history.md`.
