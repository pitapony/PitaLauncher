# PitaLauncher

**PitaGames** — a Rust + Dioxus 0.7 game launcher built for CachyOS and Arch
Linux.

> The repository is named **PitaLauncher**; the product's working name in the
> design documents is **PitaGames**. Linux-only by design: Windows, macOS and
> Flatpak are explicit non-goals.

## Status

**M0 vertical slice shipped and container-verified. M1 in progress.**

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
- **`pitagames-shim`** — argv-only env injection and exit-code forwarding.
- **Headless CLI** — `scan`, `install`, `metadata`, `proton`, `launch-options`.

## Not there yet

- Hardware validation on the RX 9070 and RX 5700 XT.
- GUI Proton picker and per-game override.
- Compatibility database, anti-cheat detection and injection gating.
- Graphics/upscaler orchestration (FSR 4, OptiScaler, DLSSNR, HelixSR).
- Other stores (GOG, Epic, Amazon), mod orchestration and packaging.

## Requirements

- CachyOS or Arch Linux, x86_64.
- Rust 1.85+ (the workspace uses edition 2024).
- For the GUI: `webkit2gtk-4.1`, `gtk3`, `libsecret`, `sqlite`.
- Steam installed natively. Flatpak Steam is unsupported.
- Target GPUs: RX 9070 (RDNA 4) and RX 5700 XT (RDNA 1). Everything else ships
  as experimental/unverified and is labelled "not tested by us".

## Quick start

```bash
# Headless scan of the installed Steam library.
cargo run -p pitagames-cli -- scan

# List the Proton builds this machine can launch games with.
cargo run -p pitagames-cli -- proton

# See the shim line the launcher would write, without touching the file.
cargo run -p pitagames-cli -- launch-options 620 --shim --dry-run

# Build and run the GUI (needs the WebKitGTK dependencies above).
cargo run -p pitagames
```

The CLI also supports `install <appid>` (hands the request to Steam and polls the
appmanifest), `metadata <appid>` (live Steam store metadata) and
`launch-options <appid> --set/--shim/--clear/--restore`.

## Build and test

The GUI links against WebKitGTK and is built on Arch/CachyOS (or an
`archlinux:latest` container). The pure-Rust crates build anywhere.

```bash
# Pure crates: core, db, art, metadata, platform, stores, shim, cli.
cargo test -p pitagames-core -p pitagames-db -p pitagames-art \
  -p pitagames-metadata -p pitagames-platform -p pitagames-stores \
  -p pitagames-shim -p pitagames-cli
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
| `crates/stores` | Steam VDF/ACF scan, watcher, install monitor, launch-options writer |
| `crates/metadata` | Steam `appdetails` client, rate limiter and cache policy |
| `crates/platform` | Proton discovery and runtime selection |
| `crates/art` | Art download, cover-crop, WebP cache and loopback server |
| `crates/shim` | `pitagames-shim` launch shim |
| `crates/cli` | Headless developer CLI |
| `crates/app` | Dioxus desktop GUI (the only crate allowed to use RSX) |
| `fixtures/` | Steam VDF/ACF and `localconfig.vdf` fixtures |
| `docs/` | Per-slice implementation notes and the hardware test checklist |

## Documentation

- [`docs/M0-test.md`](docs/M0-test.md) — the hardware launch checklist.
- [`docs/m1-detail-page.md`](docs/m1-detail-page.md) — game detail page and profiles.
- [`docs/m1-metadata.md`](docs/m1-metadata.md) — store metadata and caching.
- [`docs/m1-art.md`](docs/m1-art.md) — art pipeline and loopback server.
- [`docs/m1-proton.md`](docs/m1-proton.md) — Proton discovery and selection.
- [`docs/m1-launch-options.md`](docs/m1-launch-options.md) — Steam launch-options writer.
- [`AGENTS.md`](AGENTS.md) — contributor and agent constraints.

## License

MIT. See [`LICENSE`](LICENSE).
