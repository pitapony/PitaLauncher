# M2 — the Epic backend (`stores::epic`, driven by `legendary`)

Status: implemented and verified against fake CLIs locally (macOS) and in
`archlinux:latest`. **Not yet driven against a real `legendary`**: it is not
installed on either target machine, and Epic's own service is only reachable
from those machines. The argv templates, JSON shapes and summary lines below
were taken from legendary's source (`cli.py`, `models/game.py`,
`downloader/mp/manager.py`), not from a live run.

## Why a CLI

Epic has no public library/download API a third-party launcher may call; the
official EGS client talks to endpoints it does not document. That leaves
wrapping an existing CLI, exactly like the plan does for Amazon (`nile`) and GOG
(`lgogdownloader`). [`legendary`](https://github.com/legendary-gl/legendary) is
the mature one for Epic: Python, maintained, `--json` on the read paths, and it
already owns the auth token store, the manifests and the CDN download logic.

So this slice is the Epic *half* of that: the argv presets, the JSON shapes, the
verify classification, and one CLI surface (`pitagames-cli epic …`) that lets
the machine-level behaviour be exercised without the GUI. The driver itself
(`stores::cli`) landed first and is documented in `docs/m2-cli-backend.md`; the
GUI's store status row and library sync build on top of these functions and are
a separate M2 item.

```text
  Epic::new() ──spec()──► CliSpec ──► CliHandle (PTY, timeouts, transcript)
      │                                   │
      ├─ status()      resolve_program     └─ Percent / Line events
      ├─ version()     --version
      ├─ account()     status --offline --json
      ├─ owned()       list --json
      ├─ installed()   list-installed --json
      └─ stream(cmd)   install / verify / launch / auth / logout
```

## Shape

| Item | Role |
|---|---|
| `epic::PROGRAM` | `"legendary"` |
| `epic::spec()` / `spec_with_program()` | the `CliSpec` preset (templates below) |
| `epic::PROGRESS_PATTERN` | the one regex matching both progress line families |
| `Epic` | facade: `new`, `with_program`, `status`, `spawn`, `output`, `stream`, `owned`, `installed`, `account`, `version` |
| `StoreStatus` | `Ready { program }` or `CliMissing { program }` — the first state a store row shows |
| `RunReport` | `command`, `outcome`, `text`, `errors`, `warnings`, `succeeded()`, `exited_cleanly()`, `failure_message()`, `log_path()` |
| `VerifyReport` | `verified`, `corrupted`, `missing` — parsed, never taken from the exit code |
| `EpicAccount` | `account`, `games_available`, `games_installed`, `config_directory` |
| `EpicError` | `NoJson`, `Json`, `Failed { command, message }`, `Cli(..)` |
| `epic::install_dir(app)` | `<XDG_DATA_HOME>/pitagames/games/epic/<safe app name>` (`core::paths::games_dir()` + one sanitized component) |

`Epic` is a thin facade on purpose: it holds no SQLite handle and no UI state,
so the same calls serve the CLI today and the GUI's sync worker later.

## Argv

Every template is a `Vec<String>`; nothing is a shell string. The driver
substitutes `{id}`, `{dir}`, `{parent}` and `{name}` in one pass.

| `CliCommand` | argv | notes |
|---|---|---|
| `ListOwned` | `list --json` | array of `vars(Game)`, DLC nested inside each game |
| `ListInstalled` | `list-installed --json` | array of `vars(InstalledGame)` |
| `Status` | `status --offline --json` | offline: "logged in?" must not need the network |
| `Version` | `--version` | `legendary version "0.20.35", codename "…"` |
| `Auth` | `auth --disable-webview` | the webview cannot run in a launcher and cannot be answered over a PTY |
| `Logout` | `auth --delete` | removes the stored session |
| `Install` | `install {id} --base-path {parent} --game-folder {name} --yes --disable-sdl` | see "Install path authority" |
| `Verify` | `verify {id}` | exits 0 either way (below) |
| `Launch` | `launch --skip-version-check {id}` | no update check, no network login |
| log files | `cli-legendary-<command>-<nanos>-<pid>.log` | `list-owned`/`list-installed`/`status`/`version`/… |

**Install path authority.** legendary installs into `<--base-path>/<--game-folder>`,
so passing the requested directory as `--base-path`/`--game-folder` keeps
PitaGames' `install_dir` authoritative instead of letting legendary pick
`~/Games/…`. `{parent}`/`{name}` come from splitting that directory, and the
driver refuses the template when there is no usable parent (`CliError::Template`)
rather than silently installing somewhere else. `--yes` answers "do you wish to
install", `--disable-sdl` skips the interactive install-tag prompt (selective
download would otherwise need a human to pick language/DLC parts). The default
directory sanitizes the app name to a single path component (`/`, `\`, control
characters → `-`; `.` / `..` / empty → `epic-game`), because the name comes from
legendary's JSON or a command line.

## JSON, through a PTY

A PTY has one stream, so legendary's `[cli] INFO: …` log lines (a
`StreamHandler` on stderr) interleave with the `--json` document. `parse_owned`,
`parse_installed` and `parse_account` therefore try, in order: the whole text as
one document (the normal case — the `--json` path prints nothing else), the text
with log lines removed, then the span from the first `[`/`{` line to the last
`]`/`}` line. Unknown fields are ignored, so a legendary update that adds fields
does not break the parse.

The fixtures in `fixtures/legendary/` are the shapes actually printed, including
a DLC entry in both `list` (nested and top-level) and `list-installed`:

- `list.json` — two games plus one DLC. DLC is identified by
  `metadata.mainGameItem`; those rows are dropped, so a pack does not appear as
  a second library entry with its parent's art.
- `list-installed.json` — one game plus one DLC (`is_dlc: true`). The game
  contributes `install_dir`, an `exe` (legendary's `executable` is relative to
  the install dir), `size_bytes`, `version` and tags.
- `status.json` — `account`, `games_available`, `games_installed`,
  `config_directory`; `<not logged in>` means no stored session.

`Game` has no platform field, and "Windows vs native" is what decides whether a
title can run without Proton at all, so `InstalledEntry::into_game` keeps it as
a tag: `platform:windows` next to `install_tags`, plus `needs-verification` when
legendary flags the install. Installing, updating and launching use those tags;
no schema change is needed to display them.

## Progress

```text
[DLManager] INFO: = Progress: 12.34% (1234/10000), Running for 00:01:23, ETA: 00:00:09
Verification progress: 123/456 (12.3%) [1.2 MiB/s]
```

Both are matched by `PROGRESS_PATTERN`, and nothing else is. That narrowness is
deliberate: the install summary also prints a percentage
(`Download size: 1234.50 MiB (Compression savings: 37.5%)`), and a generic
`(\d+)%` regex would yank the bar back to 37 % after the download finished. A
line that does not match stays a plain event, so a parser miss degrades to text,
never to a wrong bar.

## Verify: the exit code is not the answer

`legendary verify` exits `0` whether the install is intact, corrupt or missing
files. `verify_report` therefore classifies the text:

- `Verification finished successfully.` → `verified`, 0/0
- `Verification failed, 3 file(s) corrupted, 1 file(s) are missing.` → not
  verified, 3/1
- neither line (a refusal before the scan, e.g. a missing manifest) → not
  verified, 0/0

`pitagames-cli epic verify` prints the verdict, still shows the transcript path,
and exits non-zero when the verdict is not a pass — so a script can gate on it
even though legendary cannot.

## The exit-0 traps

Two legendary behaviours would make a launcher report success for a failed
action, so `RunReport` adds a second signal:

- `succeeded()` = exit `0` **and** no line logged at `ERROR`/`CRITICAL`/`FATAL`.
  legendary logs an error and exits `0` for some refusals (a title it says must
  be installed through its own third-party store, for example). The log levels
  are legendary's own `[name] LEVEL:` format, so ordinary prose cannot trigger
  it.
- `exited_cleanly()` = exit `0` only. This is the verdict for `launch`: the game
  writes to the same PTY, and an engine logging `[SomeEngine] ERROR: …` on its
  way out must not turn a clean launch into a failed one. The launch's exit code
  is the game's own.

On failure the CLI prints the transcript path to stderr, because the interesting
line ("store is offline", "manifest missing") is usually in the full log, not in
the last event.

## Auth

`auth --disable-webview` is the code-paste flow: legendary prints a login URL
and asks for the `authorizationCode` from the page's JSON response. That is an
`input()` prompt with no trailing newline, which is exactly what the driver's
partial-line flush exists for. The CLI relays one line of stdin for every prompt
line ending in `:`, and the run uses `CliOptions::interactive()` — no idle
timeout (a person needs minutes to fetch a TOTP code), 10-minute hard cap.

**A PTY echoes typed input**, so the transcript on disk can contain the code.
That log is a local diagnostic: it is never printed, never stored in SQLite and
never shown in the UI. Redaction is an M4 hardening item. Ctrl-D on the prompt
cancels the run (the driver sends `SIGHUP`, then `SIGKILL` to the group).

## Status and version

`status --offline --json` answers "is this account signed in?" without the
network, which is what the store row needs before offering an install.
`--version` is parsed for the "outdated legendary" warning later. Both probes
run only when the program resolved; `pitagames-cli epic status` prints one
`problem:` line naming whichever probe failed, and `--json` the same as fields.

## CLI surface

```bash
pitagames-cli epic status [--json]
pitagames-cli epic list [--json]
pitagames-cli epic installed [--json]
pitagames-cli epic auth
pitagames-cli epic logout
pitagames-cli epic install <app> [--dir <path>] [--dry-run]
pitagames-cli epic verify <app> [--json]
pitagames-cli epic launch <app> [--dry-run]
```

Shared flags: `--program <path>` (a wrapper script or a fixture CLI),
`--log-dir <path>`, `--dry-run` (prints the resolved argv instead of running
it), `--json`, `-h/--help`. `--dry-run` is the safe first step on a new machine:
it shows the exact argv, including where the install would go.

The `install` default directory is
`$XDG_DATA_HOME/pitagames/games/epic/<app>`; `--dir` overrides it.

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-stores -p pitagames-cli
cargo clippy -p pitagames-stores -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`epic::tests` (12) covers: the documented argv of every command, the install
placeholder split, both progress line families plus the install summary that
must *not* match, owned/installed parsing around log noise and DLC filtering,
compact single-line JSON, the two distinct parse errors, verify classification
(success / corrupt+missing / no summary), account and version parsing, the
sanitized install dir, the missing-vs-present CLI status, an exit-0 run that
logged an error being a failure, a launch whose game logged an error still being
a clean exit, and one end-to-end fake-CLI run.

The fake CLIs are `#!/bin/sh` scripts injected through `CliOptions::env["PATH"]`
(POSIX built-ins only, so they behave the same in the Arch container). Manually,
a fake `legendary` exercised `status`, `list`, `installed`, `install` (with
`--dir` and `--dry-run`), `verify` (pass and fail), `launch`, `auth` through
`write_stdin`, and `logout`; the per-run logs landed in
`$XDG_DATA_HOME/pitagames/logs`.

## Known limits

- **Not yet run against a real `legendary`.** The fixtures come from its source;
  the first real-machine run (install one free game, verify it, launch it) is
  the closing check for this slice, like the M0 hardware closer.
- **CLI runs in `$HOME`.** The driver keeps `CliOptions::cwd = None` unless a
  caller sets it, and `portable-pty` then starts the child in the home directory.
  That is right for `legendary` (its config comes from XDG, install paths are
  absolute) and it is why a relative `--program ./legendary` and a fake CLI that
  reads a fixture with a relative path both fail: use absolute paths.
- **The game runs through legendary's own Wine/Proton configuration.** The
  umu/Proton prefix handoff, `DXVK_HUD`, the shim profile and the wrapper chain
  are the later M2 prefix item; `launch` is deliberately the plain call.
- **No DB writes and no GUI.** `owned()`/`installed()` return domain values; the
  store status row, the owned-games cache and the Downloads board wiring for
  Epic are the next slice on top.
- **DLC is dropped, not modelled.** A pack is not a library row; per-DLC install
  state is not represented yet.
- **No update / uninstall / repair UI.** `verify` exists; `legendary update` and
  `legendary uninstall` have no command here yet.
- **No `--json` progress.** legendary prints percent lines, so `Percent` is
  enough; `JsonLines` stays for a backend that offers it.
- **Auth codes can sit in a transcript** until redaction lands in M4.

## Deliberately not in this slice

- The `StoreBackend` trait, the owned-games cache rows and the store status UI.
- umu/Proton handoff, the shim profile, artwork lookup (Epic catalogue art needs
  its own source) and anti-cheat detection for Epic titles.
- Amazon (`nile`) and GOG (`lgogdownloader`) presets — same module shape, next
  slices.
- A `legendary` version floor/update prompt: parsed, not enforced.
