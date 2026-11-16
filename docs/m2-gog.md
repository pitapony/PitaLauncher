# M2 — the GOG backend (`stores::gog`, driven by `lgogdownloader` + `gogdl`)

Status: implemented and verified against fake CLIs locally (macOS) and in
`archlinux:latest`. **Not yet driven against a real `lgogdownloader` or
`gogdl`**: neither is installed on either target machine. The argv and the
output shapes quoted below were taken from their sources (`main.cpp`,
`src/website.cpp`, `include/globalconstants.h`, `src/downloader.cpp` for
`lgogdownloader`; `gogdl/cli.py`, `gogdl/args.py`, `gogdl/auth.py`,
`gogdl/dl/managers/manager.py`, `gogdl/launch.py` for `gogdl`), not from a live
run.

## Why two CLIs

GOG has no public library API a launcher may call, and both candidate CLIs are
partial. The plan said "prefer `lgogdownloader` for install/verify, fall back to
`gogdl` when present". Building the slice showed that ordering does not survive
contact with what the two programs actually do, so this backend splits the work
by capability instead:

- **`lgogdownloader`** (native Arch package, C++, no Node/Python at runtime) is
  the only one that can **list the account's games with titles**
  (`--list json`), and it has the clean **browser URL + pasted redirect**
  login (`--browser-login`) and an offline login-status check
  (`--check-login-status`).
- **`gogdl`** (Heroic's downloader; Python, dependency `requests`) is the only
  one that installs a **launchable game directory** (`download <id> --path
  <dir>`), repairs it (`repair`) and **launches** it (`launch <dir> <id>`).

`lgogdownloader` downloads *installer files* (`setup_*.exe`) that still have to
be run under Wine, and it has **no launch command at all**, so a game it fetched
could not be played until the M2 prefix item landed. That is why install/verify/
launch use `gogdl` and listing/auth use `lgogdownloader`: the split is by what
each program can do, not by preference. (The plan's `nodejs: gogdl` optdepend is
also stale — current `gogdl` needs Python, not Node.)

The shared PTY driver is `stores::cli` (`docs/m2-cli-backend.md`); the shared run
verdict is `stores::report` (`docs/m2-epic.md`).

```text
  Gog::new() ──spec_for(cmd)──► CliSpec ──► CliHandle (PTY, timeouts, transcript)
      │                          │
      │  library = lgogdownloader └─ Plain lines
      ├─ owned()      --list json
      ├─ account()    --check-login-status      (exit 1 when signed out)
      ├─ auth         --browser-login           (URL + pasted redirect)
      ├─ version()    --version
      │
      │  game = gogdl
      ├─ install      --auth-config-path … download {id} --path {dir} --platform windows
      ├─ verify       --auth-config-path … repair   {id} --path {dir} --platform windows
      └─ launch       --auth-config-path … launch   {dir} {id} --platform windows
```

## Shape

| Item | Role |
|---|---|
| `gog::PROGRAM` | `"lgogdownloader"` — the library/login CLI |
| `gog::GAME_PROGRAM` | `"gogdl"` — the install/repair/launch CLI |
| `gog::GAME_PLATFORM` | `"windows"` — the `--platform` value passed to `gogdl` |
| `gog::spec()` / `spec_with_program()` | the `lgogdownloader` `CliSpec` |
| `gog::game_spec()` / `game_spec_with(program, auth_file)` | the `gogdl` `CliSpec` |
| `Gog` | facade holding both specs: `new`, `with_program`, `with_programs`, `with_programs_and_auth`, `spec_for`, `program_for`, `library_status`, `game_status`, `spawn`, `output`, `stream`, `owned`, `account`, `version`, `report` |
| `gog::auth_file()` | `$XDG_CONFIG_HOME/pitagames/gog/auth.json` — `gogdl`'s token file |
| `gog::install_dir(game)` | `<XDG_DATA_HOME>/pitagames/games/gog/<safe title>` |
| `GogAccount` | `logged_in`, with `signed_in()` |
| `GogError` | `NoJson`, `Json`, `NoStatus`, `Failed { program, command, message }`, `Cli` |

`Gog::spec_for(command)` is the dispatcher: `ListOwned`, `Auth`, `Status` and
`Version` use `lgogdownloader`; everything else uses `gogdl`. A command neither
preset supports (e.g. `Sync`, `Logout`) resolves to an empty template, so
`CliSpec::argv` reports `CliError::Unsupported` instead of running a bare
program.

## Argv

Every template is a `Vec<String>`; nothing is a shell string. The driver
substitutes `{id}`/`{dir}`/`{parent}`/`{name}` in one pass. GOG is the first
backend that needs `{dir}` in **verify and launch** as well as install, so
`CliCommand::Verify`/`Launch` now carry an optional `install_dir`; Epic's and
Amazon's templates do not mention `{dir}` and are unaffected.

| Program | `CliCommand` | argv |
|---|---|---|
| `lgogdownloader` | `ListOwned` | `--list json` |
| `lgogdownloader` | `Auth` | `--browser-login` |
| `lgogdownloader` | `Status` | `--check-login-status` |
| `lgogdownloader` | `Version` | `--version` |
| `gogdl` | `Install` | `--auth-config-path <file> download {id} --path {dir} --platform windows` |
| `gogdl` | `Verify` | `--auth-config-path <file> repair {id} --path {dir} --platform windows` |
| `gogdl` | `Launch` | `--auth-config-path <file> launch {dir} {id} --platform windows` |

**`--auth-config-path` must precede the subcommand.** It is a main-parser option
in argparse, so anything after `download`/`repair`/`launch` is parsed as that
subcommand's argument. The preset bakes the resolved path into the template
prefix; without a config directory the two tokens are simply absent, never empty.

**The install path is authoritative.** `gogdl download --path <dir>` writes the
game files directly into `<dir>` (plus the `goggame-<id>.info` manifest that
`launch` reads), unlike `lgogdownloader`, whose galaxy install would append the
API's own install dir. The default is `games/gog/<safe game title>`; `Gog` looks
the title up with `--list json` and falls back to the id.

**`gogdl launch` takes the path first, then the id** (`launch <path> <id>
--platform <os>`), which is why the launch template interpolates `{dir}` before
`{id}`.

## `--list json`: a details array, not an id/title pair

`lgogdownloader --list json` prints the array from
`gameDetails::getDetailsAsJson()`. Each top-level entry is one owned game:

```json
[
  {
    "gamename": "the_witcher_3_wild_hunt",
    "gamename_basegame": "",
    "product_id": "1207660623",
    "title": "The Witcher 3: Wild Hunt",
    "title_basegame": "",
    "installers": [ { "path": "/windows/setup_…exe", "platform": "windows" } ],
    "extras": [], "patches": [], "languagepacks": [],
    "dlcs": [
      { "gamename": "…_blood_and_wine", "product_id": "1333403780",
        "title": "The Witcher 3: Wild Hunt - Blood and Wine", "dlcs": [] }
    ]
  }
]
```

Only `product_id`, `gamename` and `title` matter for a library row (the rest is
tolerated and ignored). **DLCs are nested under their base game's `dlcs`**, so
they are never top-level rows and need no filter — unlike Epic, where DLC shares
the flat list. `product_id` is serialized as a **string** and is the numeric GOG
id `gogdl` addresses a game by everywhere; the parser also accepts a bare number
and falls back to `gamename` if a rare entry has no `product_id`. A blank title
falls back to `gamename`, then to the id.

## Parsers and verdicts

- **`parse_owned`** reads the array through `stores::report::extract_json`, which
  tolerates the progress/`Getting game names` chatter `lgogdownloader` writes to
  stderr (a PTY merges it).
- **`parse_account`** reads the status line. `--check-login-status` prints
  `Login status: Logged in` (exit `0`) or `Login status: Not logged in` (exit
  `1`); a signed-out account is **not** an error, so `account()` parses the line
  regardless of the exit code and fails only when neither line is present
  (`GogError::NoStatus`).
- **`parse_version`** takes the first version-shaped token (`3.18` or
  `LGOGDownloader 3.18`).
- **Progress is `Plain`** for both programs: `lgogdownloader` draws a
  block-character bar with no percent figure, and `gogdl` prints `[NAME] INFO:`
  log lines. Neither has a percent to feed the bar, so a line stays a line.

**`gogdl` exits non-zero on failure** (`exit(1)`), which is the opposite of
legendary and nile. So the verdicts are simpler:

- library/install/verify use `RunReport::succeeded()` — exit `0` **and** no
  error line (cheap insurance for the paths that log an error and return);
- `launch` uses `exited_cleanly()` because the game shares the PTY;
- `auth` gets the same treatment through `Gog::report`.

`gogdl`'s `logging` format is `[%(name)s] %(levelname)s: …`, the **same
bracketed layout as legendary**, so `LogFormat::Gogdl` is named for call sites
but parses identically to `LogFormat::Legendary`.

## Auth: browser URL plus a pasted redirect

`lgogdownloader --browser-login` forces the browser flow (without it the program
posts an email/password form first). It prints, on stderr:

```text
Login using browser at the following url
https://auth.gog.com/auth?client_id=…&redirect_uri=…&response_type=code&layout=default&brand=gog

Copy & paste the full url from your browser here after login is complete
URL:
```

The last line is a `std::getline(std::cin, …)` prompt with no trailing newline,
which is exactly what the driver's partial-line flush exists for. The CLI relays
one stdin line for the prompt ending in `:`; the pasted URL carries the OAuth
`code=`, which `lgogdownloader` extracts. It is the same code-paste shape as
Epic's and Amazon's flows, so a PTY echoes it: the transcript is a local
diagnostic, never printed back or stored. A failed login prints
`Failed to get auth code` and exits non-zero, so the exit code is the verdict.

**No logout.** Neither CLI can forget a session (`lgogdownloader` would need its
cookie file deleted, `gogdl` its token file), and file deletion is not a CLI
operation, so `Logout`/`Sync` stay unsupported rather than growing a side effect
the driver does not model.

## Platform and Proton

Every GOG game here is fetched as the **Windows** build (`--platform windows`)
and should be tagged `platform:windows`, the same tag Epic's and Amazon's
parsers derive and the input the runtime/graphics UI uses to decide whether
Proton is needed. A native Linux GOG build is a later refinement.

`gogdl launch` starts the game itself; without a resolved Wine binary it execs
the game's exe directly, which only works for native Linux builds. The umu/Proton
hand-off then learns a **second shape** for exactly this reason: because `gogdl`
has no install listing and its rows therefore carry no executable, a GOG row is
started by its own client (`stores::launch::client_spec` → `gogdl launch <dir>
<id> --platform windows`), supervised by `platform::launch` the same way a direct
launch is. `gogdl` still owns the Wine build, so the per-game prefix apply only
to the environment, not to `gogdl`'s runtime choice. See `docs/m2-launch.md`.

## CLI surface

```bash
pitagames-cli gog status [--json]
pitagames-cli gog list [--json]
pitagames-cli gog auth
pitagames-cli gog install <id> [--dir <path>] [--dry-run]
pitagames-cli gog verify <id> --dir <path> [--json] [--dry-run]
pitagames-cli gog launch <id> --dir <path> [--dry-run]
```

Shared flags: `--program <path>` (the `lgogdownloader` binary),
`--game-program <path>` (the `gogdl` binary), `--auth-file <path>` (`gogdl`'s
token file), `--log-dir <path>`, `--dry-run`, `--json`, `-h/--help`. `epic`,
`amazon` and `gog` share one argument parser (`parse_store_args`), so a flag
added to one is available to the others.

`verify` and `launch` require `--dir`: `gogdl` addresses an installed game by
both id and path, and there is no cross-program installed listing to derive it
from (see the limits). `install --dry-run` prints the exact `--path`, including
the title lookup, and is the safe first step on a machine with both CLIs.

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-stores -p pitagames-cli
cargo clippy -p pitagames-stores -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`gog::tests` (14) covers: both presets' documented argv, the `--auth-config-path`
prefix and its absence without a config dir, the `{dir}`-in-verify/launch guard
(a missing directory is a `Template` error, not a silent `--path`), the
`spec_for` routing (`lgogdownloader` for list/auth/status/version, `gogdl` for
install/verify/launch), owned parsing from the details array (compact document,
nested-DLC skipping, `product_id`-empty fallback), both parse errors, the login
status line as the verdict including the exit-1 signed-out case, version parsing,
the sanitized install dir, the auth-file path, missing-vs-present CLIs, an
install driven through `gogdl` and never the library program, and two end-to-end
fake-CLI runs. `cli` gained a `gog` parser test that also checks `gog_backend`
defaults both programs.

The fake CLIs are `#!/bin/sh` scripts injected through `CliOptions::env["PATH"]`
(POSIX built-ins only, so they behave the same in the Arch container). Manually,
fake `lgogdownloader`/`gogdl` scripts exercised `status --json`, `list`,
`install --dry-run` (showing the resolved `--path`), `verify`/`launch --dry-run`
(showing the `--path` and path-then-id argv), `auth` through a piped URL, and a
real `install` through the fake `gogdl`; transcripts landed in
`$XDG_DATA_HOME/pitagames/logs`. `fmt --check`, `clippy -D warnings` and the
whole pure-crate test suite are clean on macOS (stores 175, cli 32) and in
`archlinux:latest`.

## Known limits

- **Not yet run against a real `lgogdownloader`/`gogdl`.** The shapes come from
  their sources; the first real-machine run (sign in, list, install one game,
  verify, launch) is the closing check, like the M0 hardware closer.
- **No installed listing.** `gogdl` has none, and `lgogdownloader --status`
  checks *its own* configured directory and reports per-file status, not "this
  game is installed here". So `verify`/`launch` need `--dir` and installed state
  will come from the scan/DB, not this module.
- **Windows platform only.** `--platform windows` is baked in; a native Linux
  GOG build is not offered yet. A GOG row is playable now that the umu/Proton
  hand-off composes `gogdl launch` as its **store-client shape**
  (`docs/m2-launch.md`); the Play button no longer waits for an executable.
- **`gogdl` needs Python** (`requests`); it is not a self-contained Arch package
  the way `lgogdownloader` is. The status line names both.
- **No DLC selection, no update, no uninstall.** `--with-dlcs`/`--dlcs` and
  `gogdl update`/uninstall are not exposed.
- **`gogdl`'s token file is its own.** PitaGames passes the path
  (`$XDG_CONFIG_HOME/pitagames/gog/auth.json`), never Heroic's; the contents are
  `gogdl`'s to create and rewrite.
- **No artwork, DB, or GUI.** The library JSON has `icon` URLs, but the art
  pipeline only speaks Steam; the store status row and the owned-games cache are
  the next M2 slice.
- **Auth URLs can sit in a transcript** until redaction lands in M4.

## Deliberately not in this slice

- The `StoreBackend` trait, DB rows, and the store status UI.
- umu/Proton handoff, prefixes, the shim profile and the wrapper chain.
- Art sourcing, anti-cheat detection, update aggregation.
- The Heroic/Lutris/Bottles importers — next slices.
