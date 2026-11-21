# ADR-0015: Non-Steam prefixes, disk pre-check and the serial install queue

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §5 (Runtime resolution); `docs/m2-prefixes.md`; `AGENTS.md` (prefixes/disk/queue)

## Context

Steam keeps each game's Wine prefix in `steamapps/compatdata/<appid>`. Every
other store needs a prefix PitaGames owns, an install that fits on disk, and a
way to run several install jobs without the store CLIs fighting over their shared
`~/.config` session and cache state.

## Decision

- **One prefix per non-Steam game**: `<data>/pitagames/prefixes/<store>-<id>`
  (`$PITAGAMES_PREFIX_DIR` or `--root` moves the root). The id runs through
  `paths::safe_component`, so a hostile store id cannot escape the root, and the
  key is also the umu `GAMEID`. `prefix::is_initialized` reads
  `drive_c`/`system.reg`/`user.reg` so "present" and "initialized" stay distinct.
  `prefix::umu_env` returns the `GAMEID`/`PROTONPATH`/`WINEPREFIX` map `umu-run`
  takes; inspection is read-only and creates nothing.
- **Disk check is `statvfs`** (`platform::disk::space`: `available`, not `free`,
  is what an install checks); `dir_size` is a bounded, symlink-free walk (200k
  entries / depth 32) so sizing a prefix cannot hang; `check` turns a required
  size into an `ok()`/`shortfall()` verdict.
- **The install queue is serial** (`stores::queue`): a pure `InstallQueue` of
  `InstallJob`s moved through `Queued → Running → Done | Failed | Cancelled`.
  `enqueue` refuses a duplicate active id; `start_next` starts at most one job
  (otherwise `Busy`) after the disk pre-check; a refused job is marked `Failed`
  and reported `Skipped`. Epic/Amazon/GOG keep session and cache state in
  `~/.config`, so two installs at once fight — hence serial.
- **The disk figure is injected as a closure**, so `stores` does not depend on
  `platform`. The GUI drives the queue through `stores::installer` +
  `app::store_installs`.

## Consequences

- Positive: a hostile or unusual store id cannot write outside the prefix root;
  installs cannot silently fill a disk; store CLIs never collide.
- Positive: `stores` stays free of `platform`; the queue is unit-testable with an
  injected space answer.
- Negative: the queue does not survive a restart, and a job with no known size
  skips the disk pre-check (no store reports a size).

## Alternatives considered

- **Share Steam's `compatdata`.** Rejected — those prefixes belong to Steam.
- **Parallel installs.** Rejected — the store CLIs share `~/.config` state.
- **`df` for free space.** Rejected — `statvfs` is direct and needs no parsing.

## References

- `docs/m2-prefixes.md`; `crates/platform/src/prefix.rs`,
  `crates/platform/src/disk.rs`, `crates/stores/src/queue.rs`.
- plan §5 *Runtime resolution* (prefix per game).
- `docs/adr/0016`, `docs/adr/0020`.
