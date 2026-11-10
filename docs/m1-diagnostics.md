# M1 — diagnostics toggles, logs viewer and the resolved command

Status: implemented and verified locally (pure crates) and in
`archlinux:latest` (`crates/app`). Not yet exercised against a live Steam game
on the RX 9070 / RX 5700 XT.

## Why this exists

`platform::envc` already compiled the diagnostic settings into environment
variables (see `docs/m1-profile.md`), but nothing in the GUI could set them and
nothing could read the logs they (or the shim) produce. This slice closes the
M1 "logs and command explanation always available" exit criterion:

- **toggles** for the HUDs, logs and indicators, on the detail page;
- a **Logs** screen that lists and tails `<data>/logs` with a filter;
- a **resolved command** — the exact line `pitagames-shim` will run — with a
  copy button, on the detail page and in the CLI.

Everything is read-only or profile-only. Nothing here injects, installs or
touches a game directory.

## Diagnostics toggles

`core::profile::Diagnostics` is six booleans; the compiler maps them like this:

| Profile field | Environment | Note |
|---|---|---|
| `dxvk_hud` | `DXVK_HUD=fps` | the M0 proof-of-life HUD |
| `mangohud` | `MANGOHUD=1` | MangoHUD overlay (needs the `mangohud` wrapper too) |
| `proton_log` | `PROTON_LOG=1` (+ `PROTON_LOG_DIR=<logs>`) | Proton writes `steam-<appid>.log` |
| `dxvk_log` | `DXVK_LOG_LEVEL=debug` (+ `DXVK_LOG_PATH=<logs>`) | DXVK writes `<exe>_d3d11.log` etc. |
| `fsr4_indicator` | `PROTON_FSR4_INDICATOR=1` | FSR 4 on-screen indicator |
| `dlss_indicator` | `PROTON_DLSS_INDICATOR=1` | DLSS on-screen indicator |

`DXVK_LOG_LEVEL` / `DXVK_LOG_PATH` are DXVK's documented debugging variables;
when no log directory is resolvable only the level is set (DXVK then logs to
stderr, which Proton captures). The environment is emitted by `envc`; the GUI
only decides which toggles are on.

The detail page's Runtime panel embeds a **Diagnostics** editor
(`crates/app/src/diagnostics.rs` over the framework-free
`runtime::DiagnosticsDraft`): a chip per toggle showing its label and the env
it emits, plus an enabled-summary line. The draft loads from the selected
profile, is applied by **Save & write profile** (validated on a copy, like the
wrapper draft), and appears in the resolved command immediately.

## The resolved command

`core::launch::render_shell_command(env, program, args)` is the one place an
environment and an argv become a copyable POSIX line. The shim's `--explain`
and the GUI both call it, so they cannot drift:

```text
DXVK_HUD=fps PROTON_LOG=1 PROTON_LOG_DIR=/home/u/.local/share/pitagames/logs \
  pitagames-shim 620 gamescope -w 2560 -h 1440 --fullscreen -- gamemoderun %command%
```

`runtime::resolved_command(app_id, compiled)` composes `pitagames-shim <appid>`
with the compiled wrapper chain in front of `%command%`;
`runtime::resolved_preview` compiles the selected profile plus the live drafts
first. It is display-only — the launch path stays argv-only — and the panel
shows a compilation refusal (e.g. a deferred M3 graphics setting) instead of a
command.

The panel and the logs screen have a **Copy** button
(`crates/app/src/copy.rs`). Dioxus desktop has no Rust clipboard API without an
extra crate, so it uses `document::eval`: `navigator.clipboard.writeText` when
the context allows it, otherwise a hidden textarea plus
`document.execCommand('copy')`. The text is always rendered in a selectable box
too, so a refused clipboard is a manual Ctrl+C, not a dead end.

## Logs viewer

`platform::logs` (framework-free, paths passed in) turns the log directory into
data:

- `list(dir) -> Vec<LogFile>` — regular files only, newest first, each with its
  size, modification time and a `LogKind` inferred from the name (`shim.log` →
  shim; `steam-<appid>.log` → proton; `<exe>_d3d11.log` / `_dxgi.log` → dxvk).
  A missing directory is an empty list.
- `tail(path, max_lines, needle) -> Tail` — reads at most `TAIL_BYTES`
  (256 KiB) from the end of the file, drops a byte-truncated first line, filters
  by an optional substring and keeps the last `max_lines`. `truncated` means the
  view is not the whole file. A many-gigabyte game log never blocks the UI.

The GUI's **Logs** tab (`crates/app/src/log_view.rs`) lists the directory, tails
the selected file (2000 lines, filtered in memory, 400 rendered), shows the
log path with a copy button, and can hand the folder to `xdg-open`. The CLI has
the same reader headless:

```bash
pitagames-cli logs                                  # list <data>/logs
pitagames-cli logs --file shim.log --tail 50        # last 50 lines
pitagames-cli logs --file steam-620.log --grep error
pitagames-cli logs --dir /tmp/logs --json
```

`--json` prints `{ dir, files }` for the listing and
`{ path, lines, window_lines, truncated }` for a tail.

## Piece map

| Crate | Responsibility |
|---|---|
| `core` (`core::launch`) | `render_shell_command` — env + argv → one copyable line |
| `core` (`core::profile`) | `Diagnostics` (unchanged) |
| `platform` (`envc`) | the diagnostics → env mapping (incl. DXVK logging) |
| `platform` (`logs`) | `list` / `tail` / `LogKind` |
| `app` (`runtime`) | `DiagnosticsDraft`, `resolved_command`, `resolved_preview` |
| `app` (`diagnostics`, `log_view`, `copy`) | the toggle chips, the Logs tab, the Copy button |
| `cli` | `pitagames-cli logs` |

## Guarantees and limits

- **Guaranteed:** toggles persist in the profile and appear in the shim file in
  the documented env; the resolved command matches `pitagames-shim --explain`
  (one renderer); the Logs screen and the CLI read the same files through
  `platform::logs`; tailing is bounded and never loads a whole file.
- **The Logs screen shows what exists, it does not create logs.** `shim.log`
  gets one line per launch; `steam-<appid>.log` and the DXVK logs appear only
  when the matching toggle is on and the game has run.
- **No log rotation or pruning.** The directory grows until the M4 hardening
  pass adds a size/age cap.
- **Copy depends on the webview clipboard.** `navigator.clipboard` may be
  unavailable under the desktop custom protocol; the `execCommand` fallback and
  the selectable text are the safety nets. There is no clipboard test in CI.
- **`xdg-open` is best-effort.** A machine without a file manager shows the
  error message; the path is still copyable.
- **The DXVK log toggle uses DXVK's own variables.** DXVK has moved file logging
  to the Proton log by default, so `DXVK_LOG_PATH` is set explicitly to keep the
  per-exe files the viewer can show.
- **Hardware untested.** No live launch on the RX 9070 / RX 5700 XT has used
  these toggles yet.

## Verification

Pure crates (macOS host and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-core -p pitagames-platform -p pitagames-shim -p pitagames-cli
cargo clippy -p pitagames-core -p pitagames-platform -p pitagames-shim -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

- `pitagames-core`: `render_shell_command` renders the env prefix and the bare
  command, and round-trips quoting.
- `pitagames-platform`: `envc` sets `DXVK_LOG_LEVEL`/`DXVK_LOG_PATH` and drops
  the path without a log dir; `logs` classifies names, lists files newest-first
  (skipping directories), tails the window and drops a partial first line,
  filters by needle, and treats a missing directory / file as empty / error.
- `pitagames-cli`: `logs` argument parsing (defaults, `--tail`, unknown flags,
  `--help`) and a fixture list + tail run.
- `crates/app`: `DiagnosticsDraft` round-trips/toggles and applies to a profile;
  `resolved_command` matches the shim composition; a graphics profile reports
  its refusal instead of a command.

The GUI is verified in `archlinux:latest`: `cargo build -p pitagames
--all-targets`, `cargo clippy -p pitagames --all-targets -- -D warnings` and
`cargo test -p pitagames` (**66 tests**, up from 60 — the `runtime` draft /
resolved-command tests). An Xvfb smoke run against a fixture Steam root and a
fixture `logs/` directory (`shim.log`, `steam-620.log`, `portal2_d3d11.log`):
the **Logs** tab lists the three files with their kind + size + time and tails
the selected one (`target/smoke/shot-diag-logs.png`), and the detail page's
Runtime panel shows the six Diagnostics chips and the **Command** line
`DXVK_HUD=fps PROTON_LOG=1 PROTON_LOG_DIR=/tmp/pg/data/pitagames/logs
pitagames-shim 620 %command%` for a seeded profile
(`target/smoke/shot-diag-command.png`).

## Deliberately not in this slice

- Enforcing the inject-related diagnostics (the FSR 4 / DLSS indicators are
  emitted, but injection itself is M3 and refused by name).
- Log rotation, a support-bundle zip and cloud upload (M4 hardening).
- A per-game `dxvk.conf` editor; the M3 graphics screen owns ini overrides.
