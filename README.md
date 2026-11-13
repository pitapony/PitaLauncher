# PitaLauncher

**PitaGames** — a Rust + Dioxus 0.7 game launcher built for CachyOS and Arch
Linux.

> The repository is named **PitaLauncher**; the product's working name in the
> design documents is **PitaGames**. Linux-only by design: Windows, macOS and
> Flatpak are explicit non-goals.

## Status

**M0 vertical slice shipped and container-verified. M1 shipped; M2 (the other
stores) in progress.**

The last M0 check — launching one real Steam game with `DXVK_HUD=fps` on the
target hardware — is still blocked on machine access. Everything below the
hardware layer has been exercised in an `archlinux:latest` container.

## What works today

- **Steam library scan** — multi-library VDF/ACF parsing, install phases,
  playtime and last-played from `localconfig.vdf`.
- **Dioxus library grid** — capsule art and detail hero images served from a
  token-guarded loopback server; placeholders until files exist.
- **Downloads board** — `steam://install` hand-off and appmanifest polling with
  real byte progress.
- **Game detail page** — info cards, SQLite-backed launch profiles, and an About
  block from cached Steam `appdetails` metadata.
- **Proton discovery** — `pitagames-cli proton` scans system/user
  `compatibilitytools.d`, Steam's own builds and umu; prints the auto choice.
- **Steam launch-options writer** — Steam-closed guard, atomic replace,
  timestamped backup, JSON restore record and byte-identical or merged restore.
- **Runtime panel** — Proton picker, per-game compatibility-tool forcing, and a
  wrapper-chain editor (gamemoderun, mangohud, gamescope options and ordered
  custom `program + args` rows) with a live command preview.
- **Owned games** — an optional Steam Web API key (desktop keyring, `0600`
  fallback file) adds the games the account owns but has not installed to the
  grid, each with an Install button; the list is cached in SQLite.
- **Single-instance lock** — an `flock` on `pitagames.lock` stops a second
  launcher from racing over the database and Steam's config files.
- **Desktop integration** — a generated `.desktop` entry, a painted
  window/tray icon with Show/Quit, and an activation socket so a second launch
  raises the running window instead of looking ignored.
- **Installable package** — `packaging/PKGBUILD` (`pitagames-git`) builds the
  GUI, CLI and shim and installs the menu entry, theme icon, AppStream metadata
  and the compat database; `makepkg -si` from `packaging/`.
- **Virtualized library grid** — windowed rows (`crates/app/src/grid.rs`) mount
  only the visible tiles plus overscan, so a thousand-game library scrolls
  without a DOM node per game.
- **Compatibility database** — a shipped `compat/*.toml` of curated per-game
  upscaling rules, merged with `$XDG_CONFIG_HOME/pitagames/compat` overrides
  field by field and validated at load; `pitagames-cli compat` prints it.
- **Anti-cheat detection and the injection gate** — the detail page scans a
  game's install directory for Easy Anti-Cheat/BattlEye markers and merges them
  with the compat DB's curated value; injection on a protected game is gated
  behind an explicit confirmation. `pitagames-cli anticheat` prints the result.
- **Store backends** — Epic through `legendary`, Amazon through `nile` and GOG
  through `lgogdownloader` + `gogdl`, all on one PTY driver: auth, listing,
  install with progress, verify and launch, plus a raw per-run transcript. Every
  failure mode the CLIs' exit codes hide (they log an error and exit `0`) is
  classified from their own lines. None of them has been driven against a real
  store CLI yet.
- **Read-only importers** — the games Heroic, Lutris or Bottles already have on
  disk are listed without a store session or the CLI being installed, keeping
  the identity each store uses so a scan does not duplicate them; nothing is
  launched, downloaded or written.
- **`pitagames-shim`** — argv-only env injection and exit-code forwarding.
- **Headless CLI** — `scan`, `install`, `metadata`, `proton`, `compat`,
  `anticheat`, `profile`, `logs`, `launch-options`, `compat-tool`,
  `steam-account`, `desktop-entry`, `activate`, `epic`, `amazon`, `gog`,
  `import`.

## Not there yet

- Hardware validation on the RX 9070 and RX 5700 XT.
- Graphics/upscaler orchestration (FSR 4, OptiScaler, DLSSNR, HelixSR) and the
  graphics screen that enforces the anti-cheat gate.
- Other stores (GOG, Epic, Amazon), mod orchestration, and release/AUR
  packaging (the `-git` package builds and installs today).

## Requirements

- CachyOS or Arch Linux, x86_64.
- Rust 1.89+ to build (MSRV: edition 2024 let-chains, `File::try_lock`). The
  package pulls the runtime stack: `webkit2gtk-4.1`, `gtk3`, `openssl`,
  `xdotool`, `libayatana-appindicator`, `xdg-utils`.
- Steam installed natively. Flatpak Steam is unsupported.
- Target GPUs: RX 9070 (RDNA 4) and RX 5700 XT (RDNA 1). Everything else ships
  as experimental/unverified and is labelled "not tested by us".

## Quick start

```bash
# Build and install the package (pulls the WebKitGTK/GTK dependencies).
cd packaging && makepkg -si

# Headless scan of the installed Steam library.
pitagames-cli scan

# List the Proton builds this machine can launch games with.
pitagames-cli proton

# Check the Steam account the launcher resolved (add --refresh to call the API).
pitagames-cli steam-account

# List the compatibility database (add --shipped ./compat for a checkout).
pitagames-cli compat

# Detect a game's anti-cheat and show the injection gate (any install dir works).
pitagames-cli anticheat 1172470 --dir /path/to/game

# See the shim line the launcher would write, without touching the file.
pitagames-cli launch-options 620 --shim --dry-run

# Start the GUI (cargo run -p pitagames from a checkout).
pitagames
```

From a checkout, prefix any CLI command with `cargo run -p pitagames-cli --`
(e.g. `cargo run -p pitagames-cli -- scan`).

The CLI also supports `install <appid>` (hands the request to Steam and polls the
appmanifest), `metadata <appid>` (live Steam store metadata),
`anticheat <appid>` (anti-cheat markers + the injection gate),
`launch-options <appid> --set/--shim/--clear/--restore`, and
`steam-account --set-key/--forget-key/--refresh`.

## Build and test

The GUI links against WebKitGTK and is built on Arch/CachyOS (or an
`archlinux:latest` container). The pure-Rust crates build anywhere.

```bash
# Pure crates: core, db, art, metadata, platform, stores, secrets, shim, cli.
cargo test -p pitagames-core -p pitagames-db -p pitagames-art \
  -p pitagames-metadata -p pitagames-platform -p pitagames-stores \
  -p pitagames-secrets -p pitagames-shim -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check

# GUI, inside Arch:
sudo pacman -S --needed rust webkit2gtk-4.1 gtk3 libsecret sqlite pkgconf openssl xdotool
cargo build -p pitagames --all-targets
cargo clippy -p pitagames --all-targets -- -D warnings
cargo test -p pitagames
```

CI runs the same commands in `.github/workflows/ci.yml`.

## Repository layout

| Path | What it is |
|---|---|
| `crates/core` | Domain types, launch spec, profiles, XDG paths, metadata types |
| `crates/db` | SQLite pool, migrations and repositories |
| `crates/stores` | Steam VDF/ACF scan, watcher, install monitor, launch-options and compatibility-tool writers, the PTY CLI driver and the Epic backend |
| `crates/metadata` | Steam `appdetails` and `GetOwnedGames` clients, rate limiter and cache policy |
| `crates/platform` | Proton discovery, runtime selection, the compatibility DB loader, anti-cheat detection and the runtime env compiler |
| `crates/art` | Art download, cover-crop, WebP cache and loopback server |
| `crates/secrets` | Steam Web API key storage (keyring + `0600` fallback) |
| `crates/shim` | `pitagames-shim` launch shim |
| `crates/cli` | Headless developer CLI |
| `crates/app` | Dioxus desktop GUI (the only crate allowed to use RSX) |
| `compat/` | Shipped compatibility database (`*.toml`, one game per file) |
| `packaging/` | `PKGBUILD`, `.desktop`, theme icon, AppStream metainfo |
| `fixtures/` | Steam VDF/ACF, `localconfig.vdf` and `config.vdf` fixtures, plus `legendary` JSON shapes |
| `docs/` | Per-slice implementation notes and the hardware test checklist |

## Documentation

- [`docs/M0-test.md`](docs/M0-test.md) — the hardware launch checklist.
- [`docs/m1-detail-page.md`](docs/m1-detail-page.md) — game detail page and profiles.
- [`docs/m1-metadata.md`](docs/m1-metadata.md) — store metadata and caching.
- [`docs/m1-art.md`](docs/m1-art.md) — art pipeline and loopback server.
- [`docs/m1-proton.md`](docs/m1-proton.md) — Proton discovery and selection.
- [`docs/m1-compat-db.md`](docs/m1-compat-db.md) — the compatibility database loader, schema and merge rules.
- [`docs/m1-anticheat.md`](docs/m1-anticheat.md) — anti-cheat detection and the injection gate.
- [`docs/m1-launch-options.md`](docs/m1-launch-options.md) — Steam launch-options writer.
- [`docs/m1-profile.md`](docs/m1-profile.md) — shim profile contract and runtime panel.
- [`docs/m1-compat-tool.md`](docs/m1-compat-tool.md) — per-game compatibility-tool writer.
- [`docs/m1-wrappers.md`](docs/m1-wrappers.md) — wrapper-chain editor and argv flattening.
- [`docs/m1-owned-games.md`](docs/m1-owned-games.md) — Steam Web API key and the owned-games cache.
- [`docs/m1-single-instance.md`](docs/m1-single-instance.md) — the single-instance lock.
- [`docs/m1-virtualized-grid.md`](docs/m1-virtualized-grid.md) — the windowed library grid.
- [`docs/m1-desktop-integration.md`](docs/m1-desktop-integration.md) — `.desktop` entry, tray icon and single-instance focus.
- [`docs/m1-packaging.md`](docs/m1-packaging.md) — the PKGBUILD, AppStream metadata and theme icon.
- [`docs/m2-cli-backend.md`](docs/m2-cli-backend.md) — the PTY CLI driver shared by the store backends.
- [`docs/m2-epic.md`](docs/m2-epic.md) — the Epic backend (`legendary`).
- [`docs/m2-amazon.md`](docs/m2-amazon.md) — the Amazon backend (`nile`).
- [`docs/m2-gog.md`](docs/m2-gog.md) — the GOG backend (`lgogdownloader` + `gogdl`).
- [`docs/m2-importers.md`](docs/m2-importers.md) — the read-only Heroic, Lutris and Bottles importers.
- [`AGENTS.md`](AGENTS.md) — contributor and agent constraints.

## License

MIT. See [`LICENSE`](LICENSE).
