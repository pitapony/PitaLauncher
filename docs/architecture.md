# Architecture

PitaGames is a Rust workspace with one GUI and a framework-free core. This page
explains the crate boundaries, the vocabulary the code and the docs share, and
the rules a change must respect. It matches the vocabulary in
[`AGENTS.md`](../AGENTS.md), which is the source of truth; the design and the
milestones live in the plan outside the repository, and every decided question is
recorded in [`docs/adr/`](adr/README.md).

The target is **CachyOS/Arch Linux, x86_64, native Steam**. Windows, macOS and
Flatpak are explicit non-goals (ADR-0003). Only the **RX 9070** (RDNA 4) and
**RX 5700 XT** (RDNA 1) are validated in-house; everything else ships
`experimental` and must say "not tested by us" (ADR-0004).

## The one hard boundary

**Only `crates/app` may use Dioxus, RSX or any webview API.** Every other crate
is framework-free: it takes plain data in, returns plain data out, and can be
built, tested and run on any host without a display (ADR-0006, ADR-0007). The
only place the webview is spoken to is the Dioxus component tree; the only place
a controller event enters is a pump in `crates/app` feeding `crates/input`.

Consequences a change must respect:

- no JavaScript navigation, no DOM focus, no `localStorage`;
- art is served over a token-guarded loopback HTTP server, never `file://` or a
  base64-inlined grid;
- controller input goes through `gilrs` in Rust, not the webview Gamepad API
  (ADR-0021), which only works while focused and cannot be tested headlessly;
- no `<video>` in the v1 UI (it would bind the UI to GStreamer/WebKit and block
  the planned Blitz/`dioxus-native` migration);
- a launch is composed as **argv, never a shell string** (ADR-0017).

Because the boundary is clean, the twelve pure crates build and test on the
macOS development host; only `crates/app` needs the Arch container.

## The twelve-crate workspace

| Crate | Package | Responsibility |
|---|---|---|
| `crates/core` | `pitagames-core` | Domain types (`Store`, `GameId`, `Game`, `AntiCheat`, the launch profile), the argv-only launch composition and wrapper validation, the shim-profile contract, XDG paths, the single-instance lock, the activation socket, the `.desktop` renderer, UI/design tokens and the session-file model. The shared vocabulary lives here. |
| `crates/db` | `pitagames-db` | The SQLite pool, migrations and repositories: games, profiles, `game_metadata`, `owned_games` and the `launch_history` user-data table. |
| `crates/stores` | `pitagames-stores` | Native Steam: VDF/ACF scan with install phases, `localconfig.vdf` playtime/last-played, the steamapps watcher and install monitor, the guarded Steam-config writers, and the shared PTY **CLI driver** plus the Epic/Amazon/GOG **store backends**, the store-status row, the library sync, the read-only importers and the serial **install queue**. |
| `crates/metadata` | `pitagames-metadata` | Steam `appdetails` fetch/parse with a token bucket and cache policy, plus the `GetOwnedGames` client. |
| `crates/platform` | `pitagames-platform` | Proton discovery and selection, the non-Steam **prefix** layout, disk-space checks, the **compat DB** loader, **anti-cheat** detection and the EAC v2 gate, the **capability matrix**, the **env compiler** (`envc`), the NR backends/installer/verify/uninstall, the support bundle and the umu/Proton **launch hand-off**. |
| `crates/art` | `pitagames-art` | Art download, cover-crop, the 2× WebP cache and the token-guarded loopback server. |
| `crates/secrets` | `pitagames-secrets` | The Steam Web API key type (`SteamApiKey`) and the environment → keyring → `0600` file chain. |
| `crates/mods` | `pitagames-mods` | The mod primitives shared by every mod system: the `ModManifest` (BLAKE3 per file, pre-install tree hash, `.gl-backup`), the pinned-release fetcher, the line-preserving INI patcher and the launch-option capture. |
| `crates/input` | `pitagames-input` | The console-shell input layer: the closed `Action` set, per-device maps, `gilrs`/replay sources, the spatial `FocusGraph`, the remap session and the on-screen keyboard. |
| `crates/shim` | `pitagames-shim` | `pitagames-shim` — reads `profiles/steam-<appid>.json`, flattens wrappers through `core::launch`, applies the env and forwards the exit code. |
| `crates/cli` | `pitagames-cli` | The headless developer/read-only diagnostic CLI over every crate above. |
| `crates/app` | `pitagames` | The Dioxus 0.7 desktop GUI — the only crate with RSX. |

`fixtures/` holds the VDF/ACF and store-CLI fixture shapes; `compat/` is the
shipped compat DB; `packaging/` the PKGBUILD and the generated `.desktop`/
session files.

## How a game is scanned, shown and launched

```
Steam library (VDF/ACF, localconfig.vdf)
        │  stores::steam          read-only, side-effect free
        ▼
   core::Game  ────────────────►  db::GameRepository (SQLite cache)
        │                                ▲
        │  art::cache + loopback server  │  metadata::appdetails
        ▼                                │
   crates/app: View::Library  ──►  View::Detail   (the game page)
        │
        │  Runtime panel: core::LaunchSpec / wrapper chain
        ▼
   profiles/steam-<appid>.json  ──►  pitagames-shim <appid> %command%
        │
        ▼
   Steam launch options (stores::steam_config, Steam-closed guard)
```

For a non-Steam store the path is different — see the hand-off shapes below — but
it shares the same `LaunchSpec`, `platform::envc` compiler and supervision.

## The vocabulary

Use these terms exactly; they are what `AGENTS.md` and the code name things.

- **Store** / **playback** — `Steam`, `Epic`, `Amazon`, `Gog`, or `Local`
  (imported/side-loaded); a `GameId` is `(store, store_id)`. Imported games keep
  the identity their store uses and are tagged `imported:<launcher>`.
- **Store backend** — one wrapped CLI per store (`legendary` for Epic, `nile` for
  Amazon, `lgogdownloader` + `gogdl` for GOG), each a set of argv presets on the
  shared **PTY CLI driver**. GOG is driven by two CLIs because neither one can do
  both jobs (ADR-0012).
- **CLI driver** (`stores::cli`) — one argv template per command, a PTY with
  typed stdin, parsed progress, idle/hard timeouts with process-group escalation,
  cancellation and a raw per-run transcript.
- **Run verdict** (`stores::report`) — a wrapped CLI that logs an error and exits
  `0` is **not** a success (ADR-0011): `RunReport::succeeded()` needs exit 0 *and*
  no error line; some commands use `exited_cleanly()` or a per-store marker.
- **Install queue** (`stores::queue`) — the serial queue the CLI stores share; a
  job moves `Queued → Running → Done | Failed | Cancelled`, one at a time, behind
  a disk pre-check.
- **Update** — not a second path: an install of an existing id plus that store's
  **verify**. Steam updates stay the `steam://install` hand-off (ADR-0030).
- **Prefix** — one Wine/Proton prefix per non-Steam game at
  `prefixes/<store>-<id>`; its key is also the umu `GAMEID`.
- **Hand-off** — how PitaGames starts a game. There are exactly three shapes
  (ADR-0020): **umu** (`umu-run` with `GAMEID`/`PROTONPATH`/`WINEPREFIX`),
  **native** (run the executable directly), and **store client** (`gogdl launch`
  / `nile launch`, the client owning its own runtime).
- **env compiler** (`platform::envc`) — the one place that turns a profile plus
  the capability matrix into a launch's environment and wrapper chain. The shim
  and the GUI both read its result; nothing re-implements it.
- **Capability matrix** (`platform::capability`) — the one answer per graphics
  option: available, or hidden with a reason. The same reason string is the UI's
  copy and the docs table.
- **Compat DB** — the shipped `compat/*.toml` + user overrides; read-only
  curated data (ADR-0025). "verified" is its anti-cheat/compatibility claim.
- **Anti-cheat / EAC v2** — a read-only marker walk merged with the curated
  value; the injection gate. EAC Linux v2 runs only from the CachyOS EAC entry
  (ADR-0031).
- **Injection slot** — Tier A, Tier B, HelixSR and DLSSNR are mutually exclusive
  (ADR-0023).
- **Focus graph** (`crates/input`) — the spatial layout both shells navigate; a
  `FocusId` per cell, deterministic tie-breaks and wrap.
- **Shell** — `Desktop` (frameless windowed) or `Console` (undecorated
  fullscreen); one component set, switched at runtime (ADR-0022).
- **Session** — the v1.1 login-screen path: a generated wayland-sessions entry
  and a systemd user unit running `pitagames --console --session` inside one
  `gamescope`.
- **Overlay** — the `Menu`/`Start` system menu (`Resume`, game details/store
  page, shell switch, power rows).
- **Launch hand-off (window)** — hide the window while a supervised game runs and
  bring it back when it ends.
- **Launch history** — one recorded row per supervised session, including one
  that never started (with its reason).

## Storage: SQLite for data, files for config

Two stores, by decision (ADR-0008), under the XDG directories
(`core::paths`):

| What | Where | Format |
|---|---|---|
| Games, profiles, metadata, owned games, launch history | `<data>/pitagames/library.db` | SQLite (pool + migrations) |
| The shim profile the shim reads | `<data>/pitagames/profiles/<store>-<id>.json` | JSON, written atomically |
| The compat DB | `/usr/share/pitagames/compat` + `<config>/pitagames/compat` | TOML |
| UI/display/start config | `<config>/pitagames/config.toml` | TOML |
| Input maps | `<config>/pitagames/input.toml` | TOML |
| Secrets | keyring, else `<config>/pitagames/secrets/` | `0600` file |
| Per-run logs | `<data>/pitagames/logs/` | raw transcripts, tailed read-only |
| Non-Steam prefixes | `<data>/pitagames/prefixes/` | one directory per game |
| Managed Proton builds | `<data>/pitagames/proton/` | fetched from a pinned release |
| Single-instance lock / activation socket | `<data>/pitagames/` | flock + Unix socket |

A missing file means the defaults; a malformed one is a warning plus the
defaults, never a crash.

## Rules a change must keep

- **Launch composition is argv-only** and one artifact. `core::launch` renders
  the human-readable command for the GUI and `pitagames-shim --explain`;
  `platform::envc` compiles the env. No `sh -c`, no string building.
- **Steam config writes require Steam closed**, an atomic replace, a timestamped
  backup and a byte-identical restore, with round-trip tests (ADR-0018). Reads
  are side-effect free.
- **Secrets never leak by accident.** The Steam Web API key has no `Display`, a
  redacting `Debug`, and only `masked()` reaches the UI; API errors are scrubbed.
- **One mod manifest for every mod** (`mods::manifest`): every created file,
  every original moved to `.gl-backup/`, a BLAKE3 hash per file and a pre-install
  tree hash. Uninstall verifies backups before touching the tree.
- **Never redistribute** DLSSNR weights or HelixSR artifacts; run upstream
  installers unmodified (ADR-0024).
- **`PITAGAMES_SESSION=1` suppresses the per-game gamescope wrapper** — nested
  gamescope is a bug.
- **An unsupported option is hidden, never greyed**, and carries the matrix's
  reason (ADR-0004).
- **A failed listing never deletes rows** (ADR-0014).

## Testing and verification

The pure crates build, lint and test on the development host and in CI. The
GUI's only verifiable build environment is an `archlinux:latest` container (or a
target machine), where an Xvfb smoke run drives the real UI with `xdotool`. Some
claims are only checkable by a human on real hardware — the hardware launch
checklist, the TV checklist and the beta walk — and are published as **"not
tested by us" / "not tested on a TV"** until a tester checks them
(ADR-0004). Performance budgets are a model in `core::perf`, asserted in CI where
it can be and recorded elsewhere — [`docs/m4-perf-budgets.md`](m4-perf-budgets.md).

New screens follow the app-shell session model (ADR-0027): a framework-free
model, a small Dioxus seam over a `Signal`, and a `Session` a screen reads one
handle from. New focus-driven screens register a focus graph and drain one
`ActionChannel` from the pump.

## Where to go next

- `AGENTS.md` — the vocabulary source and the non-negotiable constraints.
- `docs/adr/` — every decision and why; `docs/m*-*.md` — the per-slice
  implementation notes those decisions cite.
- [`docs/user-guide.md`](user-guide.md) — the product as a user sees it.
- [`docs/compat-contribution.md`](compat-contribution.md) — the compat-DB schema.
- [`docs/troubleshooting.md`](troubleshooting.md) — symptoms and fixes.
