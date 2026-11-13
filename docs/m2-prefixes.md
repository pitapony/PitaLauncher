# M2 — prefixes, disk space and the install queue

Status: `platform::prefix`, `platform::disk` and `stores::queue` are implemented
and verified against `tempfile` trees on macOS and in `archlinux:latest`. None of
it has been run against a real store CLI or a real game prefix yet — the first
non-Steam launch on the RX 9070 / RX 5700 XT is the closing check, like the rest
of M2.

## Why this slice exists

Three things that every non-Steam store needs, and that nothing owned yet:

1. **A prefix per game.** Steam keeps each game's Wine prefix inside its own
   library (`steamapps/compatdata/<appid>`). Epic, Amazon, GOG and the imported
   Heroic/Lutris/Bottles entries have none, and a launcher that points two games
   at one prefix overwrites their registry, saves and installed runtimes. The
   plan's rule is one directory per game:
   `<XDG_DATA_HOME>/pitagames/prefixes/<store>-<id>`.
2. **A disk-space check before an install.** A download that fills the disk
   halfway leaves a broken game directory, and a full root filesystem is a
   system-wide problem. The check has to happen before the store CLI starts, not
   after it fails.
3. **One install at a time.** `legendary`, `nile` and `gogdl` all keep session
   and cache state in `~/.config`; two installs at once fight over it. A queue
   also gives the Downloads view something honest to show for the non-Steam
   stores, which today only tracks Steam manifests.

This is foundation work for the umu/Proton launch hand-off and for the store
status UI: it is read-only, hardware-independent, and exactly what those two
pieces need as input.

## `platform::prefix` — where a game's Wine prefix goes

| Item | Role |
|---|---|
| `prefix::default_root()` | `$PITAGAMES_PREFIX_DIR`, else `<XDG_DATA_HOME>/pitagames/prefixes` |
| `prefix::game_key(game)` | `<store>-<id>`, run through the same `paths::safe_component` the install dirs use |
| `prefix::dir_in(root, game)` / `dir(game)` | `<root>/<store>-<id>` |
| `prefix::is_initialized(path)` | whether Proton has created a prefix there |
| `prefix::umu_env(game, proton, prefix)` | the `GAMEID` / `PROTONPATH` / `WINEPREFIX` map umu-run takes |
| `prefix::inspect(root, game)` | the `Prefix` report the GUI/CLI render |

```text
  prefix epic Gizmo
    root         /home/player/.local/share/pitagames/prefixes
    path         /home/player/.local/share/pitagames/prefixes/epic-Gizmo  (not present)
    size         not present
    proton       /usr/share/steam/compatibilitytools.d/proton-cachyos/proton (discovered)
    umu env
      GAMEID=epic-Gizmo
      PROTONPATH=/usr/share/steam/compatibilitytools.d/proton-cachyos/proton
      WINEPREFIX=/home/player/.local/share/pitagames/prefixes/epic-Gizmo
```

- **Prefixes live outside the game directory on purpose.** A store CLI can
  re-download or repair the game tree without touching the prefix, and a
  "verify install" cannot silently reset a game's settings.
- **The key is sanitized.** Store ids come from JSON and command lines
  (`../../etc/passwd`, a title with a slash), so the combined `<store>-<id>` is
  run through `paths::safe_component`: no separators, no control characters, and
  never `.`/`..`. `..` becomes the fallback `game`, so a hostile id cannot walk
  out of the root. The key is also the `GAMEID` umu sees, so it must stay stable
  across launches.
- **Initialization is detected, not assumed.** `drive_c`, `system.reg` or
  `user.reg` means Proton has started a prefix there; any one of them counts,
  because a prefix interrupted during creation still has the registry files.
  `present` (the directory exists) and `initialized` are separate, so the UI can
  tell "never launched" from "launched and cleaned up".
- **Nothing is created, deleted or launched.** This module only computes paths,
  detects state and sizes the directory. Creating the prefix is umu's job on the
  first launch; reclaiming the space is a later destructive action, and it needs
  the size this reports before it can offer a number.

`umu_env` returns exactly the three values this module owns; the compiler merges
them into the `LaunchSpec`. `PROTONPATH` picks the build in the plan's priority
order (the caller resolves `ProtonRef`), and `WINEPREFIX` forces the per-game
prefix instead of Wine's default `~/.wine`.

## `platform::disk` — free space and directory sizes

| Item | Role |
|---|---|
| `disk::space(path)` | `total` / `free` / `available` bytes, via `statvfs` |
| `disk::dir_size(path)` / `dir_size_limited` | bounded recursive size of a tree |
| `disk::check(path, required)` | a `SpaceCheck` (`ok()`, `shortfall()`) |
| `disk::human_bytes(bytes)` | the base-1024 formatter the CLI uses |

- **`statvfs`, not `df`.** No shell, no parsing a locale-dependent table. A
  missing path walks up to its nearest existing ancestor, so a check runs before
  the install directory exists — `space("/games/epic/not-yet-created")` still
  answers for `/games`. Only a path with no existing ancestor (or a NUL byte) is
  an error.
- **`available`, not `free`, is what an install checks.** On a machine with a
  root reservation, `free` includes blocks a normal user cannot write;
  `available` is the honest number. `SpaceCheck` compares against `available`.
- **`dir_size` is bounded and does not follow symlinks.** It walks with the same
  entry/depth caps the anti-cheat scan uses (`200_000` entries, depth `32`), so
  asking "how big is this prefix?" cannot hang on a huge game directory. A
  symlink is neither counted nor followed, so a self-referential tree cannot loop
  and a file reached through two names is not measured twice. A truncated walk
  says so (`truncated`) and its byte count is a lower bound.
- **A missing directory is `scanned = false`, not an error**, which is how the
  prefix report distinguishes "not there" from "there and empty".

## `stores::queue` — one install at a time

| Item | Role |
|---|---|
| `queue::InstallJob` | `id`, `title`, `target`, optional `size`, `state`, `progress` |
| `queue::JobState` | `Queued` / `Running` / `Done` / `Failed { message }` / `Cancelled` |
| `queue::InstallQueue` | `enqueue`, `start_next`, `finish`, `progress`, `cancel`, `retry`, `remove`, `clear_finished` |
| `queue::StartOutcome` | `Empty` / `Busy` / `Started { index }` / `Skipped { index, available, required }` |

The states and the transitions they allow:

```text
  enqueue ─► Queued ─start_next(space)─► Running ─finish(Ok)──► Done
                │                            │
                │                            └─finish(Err)───► Failed
                ├─cancel──────────────────► Cancelled ─retry─► Queued
                └─start_next(too little)──► Failed
```

- **The queue is pure.** No threads, no I/O, no store backend. The driver (the
  GUI's worker or a CLI loop) spawns the backend, forwards progress with
  `progress()`, and reports the outcome with `finish()`. The disk figure is
  injected as a closure returning free bytes, so `stores` never has to depend on
  `platform`; the real value comes from `platform::disk::space`.
- **`start_next` starts at most one job.** If something is already `Running` it
  returns `Busy`, so a double click cannot start two downloads of different
  games. A duplicate of an *active* id is refused at `enqueue` with
  `QueueError::Duplicate`; a *finished* job for the same game does not block a
  reinstall.
- **The space check can only refuse, never guess.** It runs only when the job has
  a size *and* the probe answers; an unknown answer (`None`) skips the check,
  because a wrong refusal is worse than a late failure. A refused job is marked
  `Failed` with a message naming the numbers and reported as `Skipped`, so the
  driver can move on to the next job.
- **The probe targets the nearest existing parent.** The install directory does
  not exist yet, so `InstallJob::space_root` walks up to the first directory that
  does — the filesystem the game will actually land on — and asks about that.
- **A late report cannot resurrect a row.** `finish`/`progress` only apply to a
  job that is still `Running`; a report that arrives after the user cancelled the
  backend is ignored rather than flipping a `Cancelled` row back to `Done`.
- **Progress is for the downloads row.** `JobProgress { percent, stage }` holds
  the last percentage (clamped to `0..=100`) and stage line, and is cleared when
  the job starts and when it finishes.

## CLI surface

```bash
pitagames-cli prefix <store> <id> [--root <dir>] [--proton <path>] [--json]
pitagames-cli disk <path> [--need <size>] [--json]
```

- `prefix` resolves the layout, reports whether the directory is present and
  initialized, sizes it, and prints the umu environment. `--root` overrides the
  root for one run (the same value `$PITAGAMES_PREFIX_DIR` sets globally);
  `--proton` pins the build for the environment instead of auto-detecting.
- `disk` reports a filesystem's total/free/available bytes and, with `--need`,
  whether an install of that size would fit. `--need` takes a plain byte count or
  a base-1024 suffix (`20G`, `1.5GiB`, `512M`); a fractional value with no unit
  is refused rather than silently becoming one byte.

Both are diagnostics: they read and print, and write nothing.

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-platform -p pitagames-stores -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

- `platform::prefix` (9 tests): the key combines store and id; a hostile id
  cannot escape the root; `dir_in` is root + key; `PITAGAMES_PREFIX_DIR` replaces
  the root; a missing prefix reports not-present; a present-but-uninitialized
  directory is not ready; `drive_c` (and a lone `user.reg`) makes a prefix ready;
  `umu_env` names all three values.
- `platform::disk` (10 tests): a tempdir reports positive capacity; a path that
  does not exist still resolves a filesystem; a missing absolute path is an
  error; `check` compares against `available`; `dir_size` totals files and
  directories; a missing directory is not scanned; symlinks are not followed;
  the entry and depth caps truncate; `human_bytes` is base-1024.
- `stores::queue` (15 tests): a fresh job is queued; a duplicate active id is
  refused; a finished job does not block a reinstall; one job runs at a time
  (`Busy`); an empty queue starts nothing; a short-on-space job is skipped and
  failed while the next runs; an unknown free space skips the check; a job with
  no size is never refused; the probe targets the nearest existing parent;
  `finish` only applies to a running job; a failure keeps its message; progress
  is recorded only while running and is clamped; cancel/retry round-trip;
  `remove` refuses a running job; `clear_finished` keeps active jobs.
- `pitagames-core` gains the `prefixes_dir()` path assertion; `pitagames-cli`
  gains 4 tests: the `prefix` and `disk` parsers, the size parser, and a
  `prefix`/`disk` run against a `tempfile` tree.

Manually (macOS, `target/debug/pitagames-cli`): `prefix epic Gizmo --root
/tmp/pg-prefixes --proton /opt/proton-cachyos/proton` printed the path
(`not present`), the size (`not present`) and the three umu values; after
creating `drive_c/windows` and a 2 KiB `system.reg` the same command reported
`(ready)` and `2.0 KiB in 1 file(s)`; `prefix steam 620 … --json` with no Proton
installed printed `proton: null` / `env: null` instead of failing; `disk /tmp
--need 1G` said `enough (37.9 GiB to spare)` and `--need 2T` said `NOT ENOUGH`.

## Known limits

- **Not yet run against a real store CLI or a real prefix.** The umu environment
  is the documented `umu-run` interface, and a prefix is only reported, never
  created — the first Epic/Amazon/GOG launch on hardware is the closing check.
- **The per-game override is a root, not a per-game path yet.** `--root` and
  `$PITAGAMES_PREFIX_DIR` move the whole root; a single game's prefix is not
  individually relocatable until the runtime profile carries a prefix field.
- **The queue has no consumer yet.** It is the model the GUI's install wiring and
  the Downloads view will drive; nothing spawns a backend through it in this
  slice, so the first integration is the store status UI commit.
- **`dir_size` is a walk, not a filesystem counter.** It totals regular files and
  ignores block allocation/hardlinks, so it can differ from `du` on a
  sparse/hardlinked tree. It is a display/prompt number, not an accounting one.
- **No prefix create/delete.** Both belong to the umu hand-off and the
  "reclaim space" action; deleting a prefix also destroys the game's
  save-in-prefix settings, which needs the same confirm-and-backup care as the
  Steam writers.
- **No store size is known in this slice.** `InstallJob.size` is filled by the
  store's own metadata (legendary/GOG report sizes); until the backends expose
  it, the queue's space check is a no-op for size-less jobs.

## Deliberately not in this slice

- umu/Proton launch composition (`LaunchSpec`, `PROTONPATH` resolution into a
  run, the shim profile, the wrapper chain).
- The GUI store status row, the DB rows and the downloads integration.
- Update aggregation and uninstall.
- Non-Steam artwork.
