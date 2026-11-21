# ADR-0013: Read-only, installed-only importers; Lutris via `pga.db` (overrides plan §4)

- **Status:** Accepted — **overrides** the plan's Lutris `games/*.yml` path
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §4 (importers); `docs/m2-importers.md`

## Context

Users arrive from other launchers with existing installs. Importing them is
useful, but writing into another launcher's data is dangerous and unnecessary.
The plan also assumed Lutris kept per-game YAML under `~/.local/share/lutris/games/`.

## Decision

- Importers are **read-only** and **installed-only** — they never write another
  launcher's files and never import a title the user does not have installed.
- Sources: **Heroic** (`legendaryConfig/legendary/installed.json`,
  `gog_store/installed.json`, `nile_config/nile/installed.json`,
  `sideload_apps/library.json`, plus cached `store_cache/gog_library.json` and
  nile's `library.json` for titles), **Lutris**, and **Bottles**
  (`library.yml` via `serde_norway`, read for `id`/`name`/`source`/`store`; the
  bottle path stays unread because a Windows program path needs the prefix).
- **Lutris' library is in `pga.db` (SQLite), not `games/*.yml`.** The YAML
  directory only holds runner config. Open it `SQLITE_OPEN_READ_ONLY` (with a
  create-less read-write fallback), `SELECT *` and read by column name so schema
  drift costs one field, not the import.
- Each launcher's roots include the three Flatpak copies
  (`~/.var/app/<id>/…`); CLI flags (`--heroic`/`--lutris`/`--bottles`) replace one
  launcher's roots at a time so a fixture tree can be scanned beside a real
  install.
- **Failure is per source:** a missing file is `(missing)`, an unparsable one is
  an error for that source alone (exit stays `0`), a broken title cache is a
  warning that costs titles and never rows.
- Imported games keep the identity their store uses, so a later scan cannot
  duplicate a row, and are tagged `imported:<launcher>`;
  `import::merge(library, imported)` adds only what is missing.

## Consequences

- Positive: importing is safe to run repeatedly and cannot corrupt another
  launcher's data.
- Positive: Heroic's Epic `app_name` is the same `GameId` `legendary` reports, so
  an import and a scan converge on one row.
- Negative: two new dependencies in `stores` (`rusqlite` 0.32 `bundled`, and
  `serde_norway`, the maintained `serde_yaml` fork).
- Not yet run against a real Heroic/Lutris/Bottles install.

## Alternatives considered

- **Read the plan's Lutris YAML path.** Rejected — it holds runner config, not
  the library.
- **Write imported installs back into the other launcher.** Rejected —
  read-only by policy.
- **Import owned-but-uninstalled titles.** Rejected — nothing to launch.

## References

- `docs/m2-importers.md`; `crates/stores/src/import.rs`.
- plan §4 (importers bullet, Lutris correction).
- `docs/adr/0014`.
