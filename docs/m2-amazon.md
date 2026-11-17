# M2 — the Amazon backend (`stores::amazon`, driven by `nile`)

Status: implemented and verified against fake CLIs locally (macOS) and in
`archlinux:latest`. **Not yet driven against a real `nile`**: it is not
installed on either target machine, and Amazon's own service login is only
reachable from there. The argv, the JSON/text shapes and every log line quoted
below were taken from nile's source (`cli.py`, `arguments.py`,
`api/authorization.py`, `api/library.py`, `downloading/manager.py`,
`downloading/progress.py`), not from a live run.

## Why a CLI

Amazon has no documented public API a launcher may call, and the official client
speaks endpoints it does not publish. So, like Epic via `legendary`, v1 wraps
[`nile`](https://github.com/imLinguin/nile) — a Python CLI, MIT, actively
developed (1.2.0 at the time of writing) that owns the login/device
registration, the entitlement sync, the manifest and CDN download logic, and
launching through wine.

This slice is the Amazon *half* of that: the argv presets, the parsers, the
verdicts that its exit code cannot give, and one CLI surface
(`pitagames-cli amazon …`) that exercises all of it without the GUI. The shared
PTY driver is `stores::cli` (`docs/m2-cli-backend.md`); the shared run verdict is
`stores::report` (`docs/m2-epic.md`).

```text
  Amazon::new() ──spec()──► CliSpec ──► CliHandle (PTY, timeouts, transcript)
      │                                    │
      ├─ status()      resolve_program      └─ Percent / Line events
      ├─ version()     --version
      ├─ account()     auth --status
      ├─ owned()       library list --json
      ├─ sync()        library sync
      ├─ installed()   library list --installed  +  <config>/nile/installed.json
      └─ stream(cmd)   install / verify / launch / auth / logout
```

## Shape

| Item | Role |
|---|---|
| `amazon::PROGRAM` | `"nile"` |
| `amazon::spec()` / `spec_with_program()` | the `CliSpec` preset (templates below) |
| `amazon::PROGRESS_PATTERN` | the one regex matching the progress bar line |
| `amazon::LAUNCH_SOURCE` / `LAUNCH_MARKER` | `LAUNCHER` / `Launching` — the "the game actually started" marker |
| `Amazon` | facade: `new`, `with_program`, `status`, `spawn`, `output`, `stream`, `owned`, `installed`, `sync`, `account`, `version`, `report` |
| `amazon::config_dir()` | `$NILE_CONFIG_PATH/nile` when set, else `$XDG_CONFIG_HOME/nile` |
| `amazon::installed_state(dir)` | parses nile's `installed.json` (`InstalledState` rows) |
| `InstalledListEntry` | `title` + `id`, from the text listing |
| `AmazonAccount` | `Username` / `LoggedIn`, with `signed_in()` |
| `amazon::VerifyReport` | `verified` + `errors` (no counts: see "Verify") |
| `amazon::launched(&RunReport)` | exit `0` **and** nile's `Launching` marker |
| `AmazonError` | `NoJson`, `Json`, `Failed { command, message }`, `State`/`StateJson`, `Cli` |
| `amazon::install_dir(game)` | `<XDG_DATA_HOME>/pitagames/games/amazon/<safe title>` |

`StoreStatus`, `RunReport` and `LogFormat` are shared with Epic (moved into
`stores::cli` / `stores::report` when the second backend arrived); `Amazon` is a
thin facade on purpose, holding no SQLite and no UI state, so the CLI today and
the GUI's sync worker later call the same functions.

## Argv

Every template is a `Vec<String>`; nothing is a shell string. The driver
substitutes `{id}`/`{dir}`/`{parent}`/`{name}` in one pass.

| `CliCommand` | argv | notes |
|---|---|---|
| `ListOwned` | `library list --json` | nile's cached entitlements, one array |
| `ListInstalled` | `library list --installed` | text listing; the only installed signal the CLI has |
| `Sync` | `library sync` | refreshes the cache from Amazon (needs a session) |
| `Status` | `auth --status` | always JSON, no network |
| `Version` | `--version` | `1.2.0 Robert Speedwagon` |
| `Auth` | `auth --login` | browser sign-in + pasted redirect URL |
| `Logout` | `auth --logout` | deregisters the device and drops the token |
| `Install` | `install {id} --path {dir}` | see "Install path authority" |
| `Verify` | `verify {id}` | a repair, not a checksum pass |
| `Launch` | `launch {id}` | nile picks wine/Bottles itself |
| log files | `cli-nile-<command>-<nanos>-<pid>.log` | `list-owned`/`list-installed`/`sync`/… |

**Install path authority.** nile has two path flags: `--base-path` appends its
own sanitized title to the base, while `--path` is the **exact** directory. The
preset passes `--path {dir}`, so PitaGames' directory stays authoritative with no
title-derived subfolder to reconcile. One caveat is nile's, not ours: on an
update or verify it prefers the path already recorded in `installed.json`, which
is the same directory when the first install came through us.

The default directory is `games/amazon/<safe game name>`: the CLI looks the
title up in the cached library (`amazon install <id>` shows
`games/amazon/Adios`, not `games/amazon/amzn1.adg.product.…`) and falls back to
the id when the cache has no title. The name is sanitized to one path component
by `core::paths::safe_component` (shared with Epic).

## JSON and text through a PTY

A PTY has one stream, so nile's `LEVEL [LOGGER]:\t message` lines (Python
`logging` on stderr) interleave with `--json` output.
`stores::report::extract_json` tries the whole document first, then the document
with log lines removed, then the span from the first `[`/`{` line to the last
`]`/`}` line; unknown fields are ignored, so a nile update that adds fields does
not break the parse.

`library list --json` prints nile's whole cached library. An entry is

```json
{ "id": "<entitlement id>",
  "product": { "id": "amzn1.adg.product…", "title": "Adios", "sku": "…",
               "productDetail": { "details": { "genres": ["Action"] } } } }
```

and only `product.id`/`product.title` matter here: **games are addressed by
`product.id`** everywhere else (install, verify, launch), and the entitlement
`id` is not a game id. `product.title` is `null` for some older titles whose
product ids are bare GUIDs, so the parser falls back to the id rather than
showing an empty row. There is no DLC marker in this payload, so nothing is
filtered: Amazon lists add-ons as their own products.

`library list --installed` has no JSON form (`--json` prints the whole library
and ignores `--installed`), so the installed set comes from the **text** listing
nile prints, one line per game:

```text
Adios ID: amzn1.adg.product.ab88… GENRES: ['Action', 'Adventure']
*** TOTAL 1 ***
```

The id is taken from the **last** ` ID: ` separator, so a title that contains one
still splits in the right place (the fixtures cover exactly that). Lines with no
separator — the `*** TOTAL n ***` summary, blank lines — are skipped.

## Installed: the listing plus nile's own state

The listing has titles and ids but no paths, and launching needs nile's own
record anyway. nile keeps that in `<config>/nile/installed.json`:

```json
[ { "id": "amzn1.adg.product…", "version": "2",
    "path": "/home/player/Games/nile/Adios", "size": 12345678901 } ]
```

`Amazon::installed` therefore uses the **CLI listing as the authority** for which
games are installed and the state file only to fill in `install_dir`,
`size_bytes` and `version`. A missing file means "nothing installed yet"; a
malformed one costs the paths and never the rows, because the rows come from the
CLI. `installed_state` itself still reports the parse error, so a diagnostic can
name a broken cache.

**`NILE_CONFIG_PATH` is a base directory, not the nile directory.** nile does
`os.path.join(NILE_CONFIG_PATH or XDG_CONFIG_HOME or platformdirs, "nile")`, and
`amazon::config_dir()` copies that exactly (it is also how Heroic keeps a
separate copy). Reading another tool's file is deliberate and read-only; the
alternative — asking nile for a machine-readable installed list — does not exist.

## Progress

```text
INFO [PROGRESS]:	 = Progress: 12.34 1234/10000, Running for: 00:01:23, ETA: 00:00:09
INFO [PROGRESS]:	 = Downloaded: 1234.00 MiB, Written: 1234.00 MiB
```

The bar's percentage has **no `%` sign** — `f"= Progress: {percentage:.02f} …"` —
which is why `PROGRESS_PATTERN` is `=\s*Progress:\s*([0-9]{1,3}(\.[0-9]+)?)\s`
and not a generic percent regex. The `= Progress:` prefix is also what keeps the
bar's sibling lines (`= Downloaded: 1234.00 MiB`) and any size in ordinary text
from being read as progress. A line that does not match stays a plain event, so a
parser miss degrades to text, never to a wrong bar.

## Verify is a repair

`nile verify <id>` is an alias of `install`: it loads the **local** manifest and
re-downloads files that are missing or wrong. There is no checksum summary and no
broken-files count, and `handle_install` logs `Download complete` even after an
early refusal (`download()` returning on "Unable to load manifest" or "Not enough
space available"). So `verify_report` classifies the CLI's own error lines:

- no `ERROR`/`CRITICAL`/`FATAL` line and exit `0` → `verified` (nile finished
  without giving up),
- otherwise → not verified, with the error lines attached.

`pitagames-cli amazon verify` prints that verdict, still shows the transcript
path, and exits non-zero when it is not a pass.

## The exit-0 traps (nile has more than legendary)

`nile`'s `main()` returns `0` and every handler catches its own failures by
logging `ERROR` and returning, so **almost no failure is visible in the exit
code**. Two consequences, both handled:

- **Library, install, verify, sync, auth** use `RunReport::succeeded()` — exit `0`
  *and* no `ERROR`/`CRITICAL`/`FATAL` line. The format is nile's, not
  legendary's: `INFO [LIBRARY]:\t …` (level first), which is why `LogFormat` is a
  parameter of the shared report instead of a constant.
- **Launch** cannot use the same rule: the game writes to the same PTY, so an
  engine logging `ERROR [Engine]: …` on its way out would fail a clean launch.
  It also refuses (`Game is not installed`, a missing `fuel.json`) with exit `0`
  and no marker. So the verdict is `amazon::launched()`: exit `0` **and** nile's
  `INFO [LAUNCHER]: Launching` line, which it logs immediately before the spawn
  and nothing of its own after. A refusal therefore keeps nile's error as the
  failure message, and a game that exits non-zero fails on its own code.

The interactive auth flow (`spawn` + `write_stdin` + `wait`) gets the same
verdict through `Amazon::report(command, outcome)`, because a failed device
registration is exactly an `ERROR` line with exit `0`.

## Auth: browser sign-in plus a pasted URL

The plan called this "TOTP over the PTY"; the real flow is different, and worth
stating plainly because it changes the UI. `nile auth --login` prints a login URL
and:

1. `Press ENTER to proceed` — an `input()` with no trailing newline. Answering it
   is where nile calls `webbrowser.open(<url>)`; **Amazon's 2FA/TOTP happens in
   that browser page**.
2. `Paste amazon.com url you got redirected to: ` — nile parses
   `openid.oa2.authorization_code` out of the pasted URL, registers the device
   and syncs the library.

The relay (`stores::auth::AuthRun`, `docs/m2-auth-terminal.md`) answers the
first prompt with a bare newline — the step is invisible in the in-app terminal,
which only shows the notice — then hands the second to whoever is driving:
`pitagames-cli amazon auth` reads a line of stdin, the GUI's terminal takes one
from its input row. That pasted URL
is long — thousands of characters is normal — and the driver's line buffer has no
practical limit, so it arrives as one event.

**A PTY echoes typed input**, so the transcript on disk contains the
authorization code, and the terminal displays the URL while it is typed. That log
is a local diagnostic: it is never printed back, stored in SQLite or shown in the
UI. Redaction is an M4 hardening item. Ctrl-D on the prompt cancels the run (the
driver sends `SIGHUP`, then `SIGKILL` to the group).

`--non-interactive`/`register` (the two-step Heroic-style flow) is deliberately
not used: it exists for tools that can drive a browser themselves, and this is
one prompt with one answer.

## Status and version

`auth --status` prints `{"Username": …, "LoggedIn": true}` with `<not logged in>`
as the name when there is no session; it reads the stored token only, so the
store row can ask "signed in?" offline. The fields are capitalized on the wire
and snake_case in `AmazonAccount`. `--version` is parsed for the "outdated nile"
warning later.

nile is not in the Arch repositories; the AUR `nile-git` package is currently
orphaned (1.1.0 vs upstream 1.2.0), so the status line points at the upstream
release binary as well.

## Proton and the "won't run" caveat

Every Amazon game is a Windows build, so the installed-list parser tags each
`Game` with `platform:windows` — the same tag Epic's parser derives, and the
input the graphics/runtime UI uses to decide whether Proton is needed at all.
Upstream nile's own README lists the real limitation: online titles that use
**FuelPump** "might not work". That is a per-title fact nobody can read from the
API, so it is copy for the store/detail UI (the next M2 item) rather than a
backend verdict; nothing here guesses.

`launch` is deliberately the plain call: nile picks wine or Bottles itself. The
store status row and the library sync ship next, and the GUI install flow after
them; the umu/Proton hand-off then learns a **second shape** for exactly this
reason — a row whose backend names no executable is started by the store's own
client (`stores::launch::client_spec` → `nile launch <id>`, supervised by
`platform::launch` the same way a direct launch is). nile still owns the prefix
and the Wine build, so the per-game prefix, `DXVK_HUD` and the shim profile apply
only to the environment, not to nile's runtime choice. See `docs/m2-launch.md`.

## CLI surface

```bash
pitagames-cli amazon status [--json]
pitagames-cli amazon list [--json]
pitagames-cli amazon installed [--json]
pitagames-cli amazon sync [--dry-run]
pitagames-cli amazon auth
pitagames-cli amazon logout
pitagames-cli amazon install <id> [--dir <path>] [--dry-run]
pitagames-cli amazon verify <id> [--json] [--dry-run]
pitagames-cli amazon launch <id> [--dry-run]
```

Shared flags: `--program <path>` (a wrapper script or a fixture CLI),
`--log-dir <path>`, `--dry-run` (prints the resolved argv instead of running it),
`--json`, `-h/--help`. `epic` and `amazon` share one argument parser
(`parse_store_args`), so a flag added to one is available to the other.

`--dry-run` on `install` is the safe first step on a machine with nile: it shows
the exact `--path` the game would land in, including the title lookup.

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-stores -p pitagames-cli
cargo clippy -p pitagames-stores -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`amazon::tests` (16) covers: the documented argv of every command (including
`--path`, not `--base-path`), the progress line *and* the sibling lines it must
not match, owned parsing around log noise (with a compact document and the
`title: null` fallback), the installed listing's `ID:` split with a title that
contains one, the `(INSTALLED) ` marker, the state file's path/size/version, a
malformed state file (error) versus a missing one (empty), the `NILE_CONFIG_PATH`
quirk, both parse errors, account/version parsing, verify classification,
`launched()` for a refusal and for a real launch whose game logged an error, the
sanitized install dir, missing-vs-present CLI, an exit-0 run that logged an error
being a failure, and two end-to-end fake-CLI runs (`owned`, `installed` with a
fixture config tree).

`report::tests` (6) pins both log formats, the shared verdicts and the JSON
extraction. The driver gained `CliCommand::Sync`, the `Unsupported` guard for a
command with no template, and `StoreStatus` (now shared, not Epic's own).

The fake CLIs are `#!/bin/sh` scripts injected through `CliOptions::env["PATH"]`
(POSIX built-ins only, so they behave the same in the Arch container). Manually,
a fake `nile` exercised `status`, `list`, `installed`, `sync`, `install` (with
`--dir` and `--dry-run`), `verify`, `launch`, both `auth` outcomes through
`write_stdin`, and `logout`; transcripts landed in
`$XDG_DATA_HOME/pitagames/logs`. `fmt --check`, `clippy -D warnings` and the whole
pure-crate test suite are clean on macOS and in `archlinux:latest`.

## Known limits

- **Not yet run against a real `nile`.** The shapes come from its source; the
  first real-machine run (sign in, `sync`, install one Prime game, verify,
  launch) is the closing check for this slice, like the M0 hardware closer.
- **`verify` repairs.** It re-downloads from the CDN; it does not only report.
  A user who only wanted a check gets downloads, which the CLI/output says.
- **No progress for `library sync`.** nile logs pages of "Got next token" lines
  but no percentage; the UI shows stages only.
- **`installed.json` is another tool's file.** Read-only, versioned by nile's
  format of the day, and the parser ignores unknown fields; a layout change
  degrades to "installed without path/size/version", never to an empty library.
- **No artwork.** Amazon art URLs (`productDetail.iconUrl` /
  `details.backgroundUrl1`) are in the payload but the art pipeline only speaks
  Steam today; the fixtures keep the URLs so the later source can use them.
- **No DB writes and no GUI.** `owned()`/`installed()` return domain values; the
  store status row, the owned-games cache and the Downloads board wiring are the
  next slice.
- **No update / uninstall.** nile has `update` (an install alias) and
  `uninstall`; neither is exposed yet, and `list-updates --json` is not parsed.
- **`nile launch` needs its own wine/Bottles setup.** No Proton handoff, no
  wrapper chain, no `DXVK_HUD` — that is the prefix item. (The umu/Proton
  hand-off later supervises `nile launch` as its **store-client shape**; nile
  still picks the runtime, so this remains true of the runtime — see
  `docs/m2-launch.md`.)
- **Auth URLs can sit in a transcript** until redaction lands in M4.

## Deliberately not in this slice

- The `StoreBackend` trait, DB rows, and the store status UI.
- umu/Proton handoff, prefixes, the shim profile and the wrapper chain.
- Art sourcing, anti-cheat detection for Amazon titles, `list-updates` parsing.
- GOG and the Heroic/Lutris/Bottles importers — GOG shipped (`docs/m2-gog.md`),
  the importers are the next slices.
