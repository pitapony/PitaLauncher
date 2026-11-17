# M2 — the umu/Proton launch hand-off (`platform::launch`, `pitagames-cli launch`, `app::play`)

Status: implemented, two shapes. `platform::launch` is verified against fake
games, a fake `umu-run` and fixture Proton trees on macOS and in
`archlinux:latest`; the GUI is built, linted, tested and smoke-run under Xvfb in
the container, where a seeded Epic row launched a fake game from its grid card,
wrote its transcript, recorded four seconds of playtime and was stopped from the
header. The second shape — a store whose client starts a game by id — shipped
after the direct one, so **GOG and Amazon rows are playable too**.
**Not yet driven against a real store game** — no machine has umu plus a real
Epic/Amazon/GOG install yet.

## Why this slice exists

Every earlier M2 slice stopped one step short of playing a game. The importers
and the library sync write rows with an install directory and an executable, the
installer fills that directory, `platform::prefix` says where a game's Wine
prefix lives and what `umu-run` needs to use it, and `platform::envc` compiles a
profile into environment and wrappers. Nothing turned that into a process.

This slice is the seam:

```text
 grid card Play / detail Play / `pitagames-cli launch --run`
                         │
                    app::play::spawn            (worker thread, one game)
                         │
                  platform::launch::prepare      profile + prefix + argv
                         │
                  platform::launch::start        setsid, tee to a log
                         │
   PlayEvent::Started(pid) ──► header strip + card label + Stop
                         │
   PlayEvent::Finished      ──► verdict strip, playtime += duration, SQLite
```

## Two shapes — by path, and by id

A store row reaches a running game one of two ways, and `prepare()` is the only
place that decides which:

| Shape | When | argv | Runtime |
|---|---|---|---|
| **Direct** | the row names an executable | `umu-run <exe>` / `<exe>` | the launcher's: `GAMEID`/`PROTONPATH`/`WINEPREFIX` |
| **Store client** | the row names none, but its store launches by id | `gogdl launch <dir> <id>`, `nile launch <id>` | the client's own |

Every Epic row and every imported Heroic/Lutris/Bottles row takes the first
shape. GOG and Amazon take the second: `nile`'s library listing names no
executable and `gogdl` has no install listing at all, so their rows keep
`exe = None` and a Play button that asked for one could never light up.

## Composition — `prepare()`

`Handoff` is the whole input: the store's `Game` row, the compiled
`ShimProfile` (`profiles/<store>-<id>.json`), the resolved `ProtonInstall`, the
prefix root and the session flag. `prepare()` is pure apart from one `stat`.

| Row | argv | Environment |
|---|---|---|
| Windows build | `umu-run <exe>` | profile env + `GAMEID`/`PROTONPATH`/`WINEPREFIX`, then `STORE` |
| native Linux executable | `<exe>` | profile env only |

A row is a Windows build when its executable ends in `.exe` (case-insensitive)
**or** it carries the `platform:windows` tag the store backends set. The suffix
is the honest signal; the tag covers an executable that is not named `.exe`.

**The profile is the same file the shim reads.** `platform::envc` is the only
compiler, and a non-Steam game's `profiles/epic-<id>.json` has the same shape as
`profiles/steam-<appid>.json`, so a diagnostic, a profile preview and a launch
can never disagree about what `DXVK_HUD` or a gamescope chain means. A missing
or corrupt profile is ignored with a warning — the shim's own contract.

**umu's values come last, and they replace rather than duplicate.** The prefix,
the Proton path and the `GAMEID` belong to the launcher, not to a file the user
may have saved months ago, so a stale `GAMEID` in a profile cannot point two
games at one prefix. `push_env` overwrites in place, which keeps the rendered
command (the transparency panel, `--json`, `--explain`) free of duplicate
assignments while `LaunchSpec`'s "later entries win" rule still holds.

**`STORE` is umu's fixed vocabulary**: `epic`, `gog`, `amazon`, `steam`, `none`
(a local/imported game). protonfixes matches on it; an unknown value would be
worse than `none`, which simply skips the store-specific fixes.

**Wrapper chains flatten in front**, through the same `core::launch` helpers the
Steam shim uses: `gamescope -- gamemoderun umu-run /games/Gizmo.exe` is
`program` plus `args`, never a shell string. `PITAGAMES_SESSION=1` drops
gamescope steps and nothing else — nested gamescope is a bug, `mangohud` and
`gamemoderun` are still wanted.

**Refusals are named before anything is spawned** (`LaunchError`): Steam (it
launches its own games), not installed, no executable (with "sync the store's
library"), an executable that is no longer on disk (with its path), and a
Windows build with no Proton build resolved. The `stat` is there because a
verified or moved install is the most common reason a launch fails, and
`umu-run: No such file or directory` is a much worse message than the path.

## The store-client shape — `Handoff::client`

`platform` must not depend on `stores` (the disk pre-check is injected as a
closure for exactly this reason), so the second shape carries its argv in rather
than composing it. `Handoff::client` is an optional `LaunchSpec`; when the row
names no executable and one is present, `prepare()` uses it verbatim, prepends
the wrapper chain, and applies the profile's environment. `LaunchMode` grows a
third value, `StoreClient`, and `Prepared::prefix` stays `None`.

`stores::launch` is the tiny bridge that builds that spec:

```rust
pub const fn launches_by_id(store: Store) -> bool;      // gog, amazon
pub fn client_spec(backends: &Backends, game: &Game) -> Result<LaunchSpec, ClientLaunchError>;
```

It resolves the backend's `CliSpec` (`gogdl` for GOG, `nile` for Amazon),
substitutes `{id}`/`{dir}` through the **same** `CliSpec::argv` the installer
uses, and packages the result. Nothing new is templated — the launch argv is a
`CliCommand::Launch` the backend already knew how to render. A template that
needs `{dir}` (GOG's `gogdl launch <dir> <id>`) fails with `CliError::Template`
when the row carries no install directory rather than dropping the argument.

Three deliberate choices:

- **No umu variables.** `gogdl` picks its own Wine and `nile` its own
  Bottles/wine, and neither accepts a `PROTONPATH`/`WINEPREFIX` argument. Adding
  them would be a lie in the transparency panel, so the client shape carries the
  profile's environment *minus* umu's keys.
- **The client's own argv is not re-derived.** `gogdl`'s `--auth-config-path`
  (the PitaGames-owned `auth.json`, never Heroic's) and `--platform windows`
  stay exactly as `stores::gog` defined them, so `pitagames-cli gog launch` and
  the GUI cannot drift.
- **The executable still wins.** A row with an `exe` uses the direct shape even
  if its store launches by id; only a row with none falls through to the client.

## Supervision — `start()`, `Session`, `terminate()`

- The child is a session leader (`pre_exec` + `setsid`), so its pid is also its
  process-group id and cancelling reaches the whole tree: `gamescope`,
  `gamemoderun`, `umu-run`, the game.
- stdout and stderr share one file, created with its parents:
  `<logs>/<store>-<id>-<nanos>.log` (`RunOptions::default_log_for`) — the same
  directory and the same key the store CLIs use for their transcripts, so the
  Logs tab shows a game's whole history in one place.
- `Session::wait` returns an `Outcome { code, signal, duration, log }`.
  `duration` is spawn-to-reap wall clock: for a non-Steam game that is the
  playtime, because no store counts it for us.
- `Session::cancel`/`terminate(pid, grace)` send `SIGTERM` to the group, then
  `SIGKILL` after `DEFAULT_GRACE` (10 s) from a detached thread, so the caller
  never blocks and a game that exits during the grace period is unaffected.
  Proton gets the `SIGTERM` first so it can flush its prefix.
- The session is not tied to the window: a launcher that closes leaves the game
  running under its own session (and the kernel reaps it normally).

## `pitagames-cli launch` — the diagnostic

```text
pitagames-cli launch <store> <id> [--exe <path>] [--title <t>] [--dir <path>]
                      [--client <program>] [--proton <dir>] [--root <dir>]
                      [--profiles <dir>] [--db <path>] [--session] [--run]
                      [--log <path>] [--json]
```

It runs the same `prepare()` the GUI does and prints the command instead of
exec'ing it:

```text
Gizmo (epic-Gizmo) → umu
  prefix   /home/me/.local/share/pitagames/prefixes/epic-Gizmo
  command  DXVK_HUD=fps GAMEID=epic-Gizmo PROTONPATH=/usr/share/steam/compatibilitytools.d/proton-cachyos \
           WINEPREFIX=.../prefixes/epic-Gizmo STORE=epic umu-run /games/pitagames/games/epic/Gizmo/Gizmo.exe

A Game (gog-1207658930) → client
  prefix   none (the store's client owns its runtime)
  command  DXVK_HUD=fps gogdl --auth-config-path ~/.config/pitagames/gog/auth.json \
           launch '/games/pitagames/games/gog/A Game' 1207658930 --platform windows
```

The row comes from SQLite (`--db`), or from `--exe`/`--title` when the cache has
none — which is what makes the whole path testable without a store, a Proton
build or `umu-run` on the machine. A store that launches by id needs `--dir`
instead of `--exe`, and `--client <program>` swaps the client (the same idea as
`--program` on the other store commands). `--run` starts it, prints the pid and
the transcript path, waits, reports `<store>-<id>`'s verdict and **exits non-zero
when the game did**.

## `app::play` — one game at a time

Framework-free (no Dioxus types), like `store_installs` and `downloads`:

| Item | Role |
|---|---|
| `PlayEvent` | `Started { id, pid, log }`, `Finished { id, title, outcome }` |
| `Play` | the one running game plus the last verdict |
| `Play::begin(&game)` | claims the slot before the thread starts; `Err` names the game already running |
| `Play::apply` | folds a report in; a late `Finished` for another id frees nothing |
| `Play::label(id)` | `"starting"` / `"playing"` for that card, `None` for every other row |
| `Play::stop` | `SIGTERM` the running group; `false` when the pid has not arrived yet |
| `Play::last` / `clear_last` | the header strip's verdict and its Dismiss |
| `PlayChannel` | the `sender()` + `async next()` pair, same shape as `InstallChannel` |
| `play::spawn(game, sender)` | one worker thread, named `pitagames-play-<store>` |
| `play::start_game` | profile → Proton → `prepare` → `start` → `wait` |
| `runtime::client_launch` | the client spec for a row that names no executable (`None` otherwise); `runtime::compose_handoff` is the composition shared with the detail page (`docs/m2-runtime-editor.md`) |

**One game at a time** is a design rule, not a limitation of the code: two
Proton trees on one prefix is exactly what the per-game prefix layout exists to
prevent. There is a single play slot, so one Stop button always knows which
process group it signals.

**A native game never needs Proton discovery to succeed.** `play::start_game`
asks discovery for a build, but a failure is only fatal when the row is a
Windows build (`needs_proton`); otherwise `prepare()` runs a native executable
with the wrappers alone. Without that, a machine with no Proton could not start
a Linux game, which is exactly what the first Xvfb run showed ("no Proton
installation found" on an `.sh`).

**Playtime is summed client-side** and written through `persist::record_playtime`
(`set_playtime` + `last_played` = now). Steam overrules the number on its next
scan (its `localconfig.vdf` is more precise); a store game has no counter of its
own, so the sum of its sessions is its only playtime.

## The GUI side

| Where | Before | Now |
|---|---|---|
| Grid card, CLI store, installed | `installed` label | **Play** when the row has an executable *or* its store launches by id, else the label |
| Grid card, running game | — | `starting` / `playing` label instead of the button |
| Detail page | "Launching {store} games is not wired up yet." | **Play**, or `play_blocked()`'s sentence (not installed / no executable and no client) |
| Above the grid | nothing | `Playing <title>` + transcript path + **Stop**, then the last game's verdict + **Dismiss** |

`GameEntry::play_is_wired()` is still the one answer to "can this row launch";
it now returns true for Steam, for any row whose store listing named an
executable, and for an installed row whose store launches by id
(`pitagames_stores::launch::launches_by_id`). `play_blocked()` supplies the
sentence for the two cases where the button would be a lie.

A refused second Play (another game is running) and a Stop that arrives before
the pid does both land in the existing notice strip; nothing is silently
dropped.

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

- `platform::launch` (20 tests, `platform` 106 → 108): Steam is refused; an
  uninstalled row is refused before anything else; a row without an executable
  says to sync; a stale path is named; a Windows game goes through `umu-run`
  with the right prefix, `STORE`, `PROTONPATH` and `WINEPREFIX`, and a stale
  profile `GAMEID` is replaced rather than duplicated; a Windows game without
  Proton is refused; the `platform:windows` tag covers a non-`.exe` executable;
  a native game runs directly with no umu variables; a row with no executable
  and a client spec launches through the client (`StoreClient`, `prefix: None`,
  the profile still applied, no umu variables); an explicit client `cwd` beats
  the install directory and wrappers still flatten; wrappers flatten in front of
  `umu-run`; the session flag drops gamescope only; `STORE` matches umu's
  vocabulary; a finished game reports its exit code, log and playtime; a failing
  game keeps its code; cancelling stops the whole process tree; the log
  directory is created; a missing program is an `io` error, not a panic; the
  spec's env reaches the child; the default log name is the game key.
- `stores::launch` (5 tests, `stores` 244 → 249): only GOG and Amazon launch by
  id; GOG passes the install directory to `gogdl` (and fails a template before
  running when it has none); Amazon launches by id alone; a store that launches
  by executable is refused.
- `pitagames-cli` (53 tests, 49 → 53): the flag parser (`--exe=`, `--dir=`,
  `--client=`, repeatable, refused values, unknown flags); a native game
  composes without running; a native game runs and reports exit 0; Steam and a
  Windows game without Proton are refused with the right message; a GOG client
  launch composes `gogdl launch <dir> <id> --platform windows` without running
  and a `--dir`-less GOG row has nothing to compose; an Amazon client launch
  uses `--client` as the program; a `--run` of a fake `gogdl` that touches a
  marker and exits 7 reports the game's own verdict.
- `crates/app` (99 tests, 95 → 99): `play` (7 → 10 — the client spec for a GOG
  row, `client_launch` staying `None` for a row with an executable or a direct
  store, and a GOG row without a directory reporting the template error);
  `library` (4 → 5 — `play_is_wired()` per store shape, and a GOG row that
  blocks nothing).
- Xvfb smoke run in `archlinux:latest`, one seeded Epic row pointing at a fake
  `#!/bin/sh` game: the card showed **Play** (it showed `installed` before this
  slice). Clicking Play ran `/bin/sh …/smoke.sh` in its own session, the header
  said `Playing Smoke Test` with the transcript path, the card said `playing`,
  the transcript file appeared under `<data>/pitagames/logs/`, the strip then
  read `Smoke Test exited 0 after 4s`, the card said `played < 1 min` and SQLite
  held `playtime_secs = 4` with a `last_played`. A second run with a 300-second
  fake game: **Stop** killed the process, the strip read `Smoke Test stopped by
  signal 15 after 2s`, and the playtime was written.
- A second smoke run for the **store-client shape**, seeded through
  `pitagames-cli stores sync --store=amazon` against a fake `nile` (so the grid
  held two `installed` Amazon rows). The cards showed **Play** where they showed
  `installed` before this slice, and the detail page too (its non-Steam note then
  reads "Per-game runtime settings … are still Steam-only"). Clicking Play ran
  `nile launch amzn1.adg.product.ab888f57` — the child's own transcript read
  `INFO [LAUNCHER]: Launching amzn1.adg.product.ab888f57` and the fake's marker
  file appeared, proving the client's argv reached the real process — the header
  strip read `Adios exited 0 after 2s`, and the row's playtime went to
  `played < 1 min` (SQLite `playtime_secs` updated).
- `pitagames-cli launch` against a fake `umu-run` and a fake Proton tree showed
  the composed command and, with `--run`, the child's environment
  (`GAMEID=epic-Gizmo`, `PROTONPATH`, `WINEPREFIX`, `STORE=epic`, `DXVK_HUD=fps`)
  and the clean argv.

## Known limits

- **Not driven against a real store game.** No target machine has umu plus a
  real Epic/Amazon install, and none has `gogdl`/`nile` at all, so the
  store-client shape is verified against fake CLIs only.
- **A client launch is a black box.** The store's client owns the runtime, so
  the launcher cannot set `DXVK_HUD`, a Proton build or a Wine prefix for a GOG
  or Amazon game: the profile's environment is applied, but `gogdl`/`nile`
  decide which Wine they use. Their games therefore ignore the per-game prefix
  (`prefixes/gog-<id>`) the umu path uses.
- **A fresh install is playable only after a library sync.** The executable (or,
  for GOG/Amazon, the row itself) comes from the store listing, and a finished
  install records the row without one. For an exe-based store
  `pitagames-cli launch <store> <id> --exe <path>` works around it today; the
  installer should learn to carry the executable.
- **A non-Steam game's profile editor** arrived in the next slice
  (`docs/m2-runtime-editor.md`): the Profiles and Runtime panels now render for
  every store and the Command panel previews this hand-off's composition. Before
  that, such a row launched with whatever a `profiles/<store>-<id>.json` held,
  which only the CLI could write.
- **No Proton resolved means a Windows game cannot start**, even when Steam
  would pick a build. `PROTONPATH` comes from `platform::proton`; an umu-managed
  `GE-Proton` download is not implemented.
- **No launch history.** Only the last verdict is kept, and only until the next
  Play. A crash mid-session loses the playtime, because the write happens when
  the worker reports.
- **The game outlives the launcher.** Closing the window leaves the session
  running (that is what `setsid` is for); there is no "adopt the game on
  restart", so the header strip comes back empty.
- **No relation to Steam's shim yet.** A Steam game still launches through
  `steam://rungameid/<appid>` and `pitagames-shim`; this module refuses a Steam
  row on purpose.

## Deliberately not in this slice

- The gamescope console session (M2.5) — `--session`/`PITAGAMES_SESSION` already
  suppress a nested gamescope, which is the part this slice owns.
- Resuming a running game after a launcher restart, and a launch history.
- The in-app PTY terminal (M2.5) and a cancellable install (both still open).
