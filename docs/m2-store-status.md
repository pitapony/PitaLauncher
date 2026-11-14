# M2 — the store status row and the unified update list

Status: `stores::status` (the row model and the update aggregation), the
`pitagames-cli stores` diagnostic and the GUI's Stores screen are implemented.
The row model and the CLI are verified against fake CLIs on macOS and in
`archlinux:latest`; the GUI is compile-checked and unit-tested there. No real
`legendary` / `nile` / `lgogdownloader` has been driven yet — none is installed
on either target machine.

## Why this slice exists

M2 wrapped three stores behind their command-line clients, and each backend can
answer for itself whether it is usable: `legendary status --offline --json`,
`nile auth --status`, `lgogdownloader --check-login-status`. Nothing owned the
question the UI actually asks — *for each store, is it there, is the account
still valid, and what needs the user's attention?* — and nothing unified the
update signals, which every store reports differently:

- **Steam** keeps update state in the app manifest (`StateFlags`), on a game, not
  in its metadata.
- **Epic** prints `needs_verification` on its install listing, which legendary
  itself treats as "verify before updating".
- **Amazon** and **GOG** expose no update signal at all yet.

This slice is the model both the CLI and the GUI render, plus the one function
that folds those shapes into a single list. It is read-only and
hardware-independent, so it ships while the M0 hardware closer waits on machine
access, and it is the foundation the store status wiring builds on — the library
sync writes now ship (`docs/m2-store-sync.md`), the install queue driver and the
in-app re-auth terminal are still open.

## `stores::status` — the row model

| Item | Role |
|---|---|
| `status::SessionState` | `SignedIn { account }` / `SignedOut` / `Unknown` |
| `status::CliTool` | one required program: requested name, `StoreStatus`, version |
| `status::StoreStatusReport` | one store's row: tools, session, counts, problems |
| `status::RowState` | `Ready` / `CliMissing` / `SignedOut` / `Unverified` |
| `status::stores(options, list)` | the three CLI stores with default programs |

```text
  StoreStatusReport::epic(&Epic, options, list) ─┐
  StoreStatusReport::amazon(&Amazon, …)          ├─► Vec<StoreStatusReport> ─► row
  StoreStatusReport::gog(&Gog, …)                ┘        │
                                                          ├─ state()  ─► RowState
                                                          ├─ session  ─► who / re-auth
                                                          └─ owned / installed counts
```

- **Presence is cheap, the rest is not.** `CliTool::probe` only resolves the
  program against `PATH` (no process run), so a row can render instantly with
  "not installed" and fill in the version/session afterwards. `StoreStatusReport`
  does the runs; the GUI calls it off the render thread.
- **Three session values, not two.** `SignedOut` means the CLI *answered* "no
  session" and the row offers re-auth; `Unknown` means the probe failed and the
  row must not pretend the user is logged out. That distinction is the whole
  reason `SessionState` is not a `bool`.
- **A store can need more than one program.** GOG's row carries two `CliTool`s —
  `lgogdownloader` for the library/login and `gogdl` for installs — and is
  `Ready` only when both resolve. `missing_programs()` names the ones to install.
- **Counts come from wherever they are free.** `legendary status --offline`
  already reports `games_available`/`games_installed`, so Epic's row has counts
  without a listing. Amazon's `auth --status` and GOG's `--check-login-status`
  carry none, so those are `None` until the caller asks for `list = true` (two
  extra CLI runs). GOG then reports `owned` but never `installed`: its library
  CLI has no per-game install listing, and "0 installed" would be a lie.
- **Failures are per store and never fatal.** A version or account probe that
  errors becomes a string in `problems`; the row still renders and the other
  stores are unaffected. That is the "nothing breaks when a CLI is missing" M2
  exit criterion, made concrete.
- **`Account` labels.** Epic prints a username; GOG prints only a boolean, so its
  `SignedIn { account }` is empty and `SessionState::account()` returns `None`
  rather than the empty string.

## The unified update list

| Item | Role |
|---|---|
| `status::UPDATE_TAG` | `"needs-verification"` — the tag a backend puts on an out-of-date install |
| `status::UpdateSource` | `SteamManifest` / `StoreFlag` |
| `status::UpdateAvailable` | game id, title, source, optional version |
| `status::updates(games, phases)` | the sorted list |

- **One function knows both shapes.** Steam's phase lives in the
  `HashMap<appid, InstallPhase>` the scan produced, not on the `Game`; a CLI
  store's flag lives in the game's `tags`. `updates` takes the games and the
  phase map and returns one list, so a caller (the GUI) never branches on store.
- **Only installed games count**, and a Steam manifest source wins if a game
  somehow carries both. The list is sorted by title (case-insensitively) with the
  id as a tiebreak, so it is stable across runs.
- **Absent is not "up to date".** Amazon and GOG are simply not represented —
  neither prints a latest version. The list is honest about what it can see
  rather than implying the rest are current.
- **The tag is shared, not duplicated.** `stores::epic` pushes
  `crate::status::UPDATE_TAG` when legendary reports `needs_verification`, so the
  string has one definition.

## CLI surface

```bash
pitagames-cli stores [status] [--store <name>] [--list] [--json] \
    [--program <path>] [--game-program <path>] [--log-dir <dir>]
pitagames-cli stores updates [--steam-root <dir>] [--with-stores] [--json]
```

- `stores` probes all three stores and prints a row per store: the state, each
  program with its version and resolved path (or "not found in PATH"), the
  session, the counts, and any probe problem. `--store` limits it to one or more
  stores; `--program` overrides the CLI (for GOG it overrides the library CLI and
  `--game-program` the game CLI) and requires exactly one `--store`, so the
  override never lands on the wrong backend. `--list` asks for the counts.
- `stores updates` folds Steam's manifest state (from a scan of `--steam-root`,
  else the discovered root) with the CLI stores' flags into one list. The CLI
  stores are only included with `--with-stores`, because that runs their install
  listings; only Epic and Amazon have one, so GOG contributes no rows and says so
  in a note. A missing Steam root is a note, not an error.

Both are diagnostics: they run the store CLIs read-only and write nothing.
`--json` prints the model as-is (the rows derive `Serialize`).

## The Stores screen

The GUI's Stores view (`crates/app/src/stores_view.rs`) renders the same model:
one card per store with the state badge, the programs, the session and (on
request) the counts. The probe runs on a worker thread and lands over a channel,
because the CLI runs take seconds; a `use_future` drains the channel and
re-renders. Sign-in is **not** offered in-app — the auth flows are interactive
PTY sessions and the embedded terminal is M2.5 — so a signed-out store shows the
exact CLI command (`pitagames-cli <store> auth`) with a copy button. Two buttons
re-probe: **Refresh**, and **Show/Hide game counts** (the `list` flag).

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata \
  -p pitagames-platform -p pitagames-stores -p pitagames-secrets \
  -p pitagames-shim -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

- `stores::status` (12 tests): a missing program is the only thing a
  presence-less row reports and names it; Epic reads the version, the session and
  the counts the status command prints; `list` refreshes the counts from the two
  listings; a signed-out store asks for re-auth without a probe failure; a CLI
  that errors leaves the session `Unverified` with a recorded problem; Amazon
  reports its session and counts only when asked; GOG needs both programs and
  names the missing one; GOG never claims an installed count and reports no
  account label; `updates` unifies a Steam manifest with a store flag and drops a
  not-installed game; a Steam manifest wins over a stale tag; an empty library is
  an empty list; `stores` probes all three in a fixed order.
- `pitagames-cli` (42, +4): the `stores`/`stores updates` parser and its
  cross-subcommand refusals; `stores --store epic --program …` runs both render
  paths against a fake `legendary`; `stores updates --steam-root …` reads a
  synthetic library and lists only the pending update; the same command renders
  without error.
- The GUI's `crates/app` builds, lints and tests in the `archlinux:latest`
  container (`cargo build -p pitagames --all-targets`, `cargo clippy -p pitagames
  --all-targets`, `cargo test -p pitagames`, 73 tests), and an Xvfb smoke run
  (`Xvfb :99`, `dbus-run-session`, `xdotool` to click the nav) opened the Stores
  screen and screenshotted the three cards — Epic, Amazon and GOG all "not
  installed" with their install hints — with no panic in the log.

Manually (macOS, `target/debug/pitagames-cli`): with no store CLI on `PATH`,
`stores` printed all three as "not installed" and named every program; with fake
`legendary`/`nile`/`lgogdownloader`/`gogdl` on `PATH`, `stores --list` printed
`epic: ready` (legendary 0.20.35, "signed in as player", `2 owned · 1 installed`),
`amazon: ready` (nile 1.2.0, `2 owned · 2 installed`) and `gog: ready`
(lgogdownloader 3.18 + gogdl, `3 owned`, no installed count). `stores updates`
against a synthetic Steam root with one `StateFlags=12` manifest and one `=4`
printed only `Portal 2 … steam manifest`; `--with-stores` added Epic's
`Gizmo … store flag` when the fake `legendary` reported `needs_verification`, and
noted that GOG exposes no signal.

## Known limits

- **Not yet driven against a real store CLI.** The account/session/count shapes
  come from the fixtures already used by the backends; the first machine with
  `legendary`/`nile`/`lgogdownloader` installed is the closing check.
- **"Outdated" is the CLI's version, not a comparison.** The row shows the
  version the CLI prints; deciding whether it is old needs a known-good version,
  which is offline-unavailable and is not attempted.
- **The update list is only as wide as the stores allow.** Amazon and GOG
  contribute nothing until their CLIs expose a latest version (or a flag), and
  GOG cannot even list installed games through this path.
- **In-app re-auth is M2.5.** The screen names the CLI command; it does not run
  the interactive login.
- **Nothing writes to the database from this screen.** The counts are live CLI
  answers, not cached rows. The library sync now writes those rows
  (`docs/m2-store-sync.md`); the GUI's install flow and the queue consumer are
  still open.
- **GOG's two programs are probed, not their versions.** Only the library CLI's
  `--version` is read; `gogdl` has no version probe in this row.

## Deliberately not in this slice

- The install queue's GUI consumer (the library sync, `docs/m2-store-sync.md`, is
  the first half of the wiring).
- The in-app PTY terminal for auth (M2.5).
- Per-store update *application* (running the store's update command).
- SteamGridDB or artwork for imported games.
