# AGENTS.md

## What this repo is (and isn't)

- **M0 shipped, M1 in progress:** the M0 slice is complete and verified in the Arch container — `crates/core` (domain, launch, profiles, metadata types, XDG paths), `crates/db` (SQLite games/profiles/`game_metadata` cache), `crates/stores` (VDF/ACF scan with per-appid `InstallPhase`, `localconfig.vdf` playtime/last-played, steamapps watcher, `InstallMonitor` polling, format-preserving `localconfig.vdf` edits + the guarded Steam launch-options writer), `crates/metadata` (Steam `appdetails` fetch, parsing, token bucket, cache policy), `crates/platform` (read-only Proton discovery/selection order for proton-cachyos, GE, Steam and umu builds), `crates/art` (art download → cover-crop → 2× WebP cache → token-guarded loopback server), `crates/shim` (`pitagames-shim`), `crates/cli` (`pitagames-cli scan`/`install`/`metadata`/`proton`/`launch-options`), and `crates/app` (library grid with capsule art, Downloads board with install tracking, game detail page with hero art, About metadata and SQLite-backed profiles; builds+tests only in the Linux container, smoke-tested under Xvfb with screenshots in `target/shot-*.png`). The M0 hardware closer — launching one real Steam game with `DXVK_HUD=fps` — is still blocked on machine access (checklist `docs/M0-test.md`). CI lives in `.github/workflows/ci.yml`. The repo is initialized and pushed to `github.com/pitapony/PitaLauncher` (the initial commit kept its MIT `LICENSE`; `Cargo.toml` now matches). The working directory was renamed from `game-launcher` to `pitagames` on 2026-11-07; the GitHub repository name stays PitaLauncher.
- The product is **PitaGames**, a Rust + Dioxus 0.7 game launcher for **CachyOS/Arch Linux only**. Windows, macOS and Flatpak are explicit non-goals. License: MIT.
- The design — architecture, domain model, env-compiler rules, milestones, risks — lives outside the repo at `/Users/julienpohie/.opencode/plan/pitagames.md`. Read it before writing code; it is the source of truth when this file is silent.
- The working directory is now `pitagames` (renamed 2026-11-07). Use `pitagames` / `pitagames-shim` as binary names, `pitagames` as the XDG dir, and never introduce `gamel` or `game-launcher-*` identifiers.

## Environment

- This machine is macOS (`darwin`); the target is Linux (Arch/CachyOS). The GUI cannot be built or run locally. Verification runs in an `archlinux:latest` container or on the two target machines. Don't add macOS code paths to make local runs work.
- Target hardware is exactly two in-house machines: **RX 9070** (RDNA 4, 16 GB) and **RX 5700 XT** (RDNA 1, 8 GB). Everything else is `experimental`/`unverified`.

## Build & verify

- **Local (macOS host)** — build/test only the pure-Rust crates:
  `cargo test -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata -p pitagames-platform -p pitagames-stores -p pitagames-shim -p pitagames-cli`
  plus `cargo clippy --all-targets` and `cargo fmt --all` over the same list. Do not build `crates/app` locally.
- **Live store metadata** is only reachable from the Linux container on this host (raw TCP to Steam times out locally while `curl` works). `pitagames-cli metadata <appid>` is the diagnostic; fixtures cover the parser locally. Details in `docs/m1-metadata.md`.
- **Proton discovery depends on the host**, so `platform::proton` tests pass explicit roots and the CLI can too: `pitagames-cli proton` prints every root it scanned plus the auto choice, and accepts `--root`/`--steam-root`/`--json` for fixture trees. Details in `docs/m1-proton.md`.
- **Steam config writes are fixture-verified**, not live-Steam-verified yet. The writer refuses when `/proc` shows Steam running; macOS reports `Unknown`, so local smoke runs need `--force` and an explicit `--localconfig`/`--backup-dir`. Use `pitagames-cli launch-options <appid> --shim --dry-run` first. Details in `docs/m1-launch-options.md`.
- **GUI verification** runs in an `archlinux:latest` container: build `crates/app` there after installing `rust webkit2gtk-4.1 gtk3 libsecret sqlite pkgconf openssl xdotool` (clippy/rustfmt ship inside Arch's `rust` package; `libudev` arrives via `systemd-libs`; `xdotool` supplies `libxdo` which dioxus-desktop's tray/menu deps link against). The Dioxus surface is compile-checked there first; never add macOS fallbacks to make the local toolchain happy.
- cargo on this machine needs `CARGO_HTTP_MULTIPLEXING=false` (libcurl stalls against the crates.io index; the sparse index then caches normally).

## Non-negotiable design constraints

- **Renderer boundary:** only `crates/app` may use Dioxus/RSX. `core`, `db`, `stores`, `art`, `platform`, `mods`, `metadata`, `input`, `shim`, `cli` stay framework-free. No JS navigation, no DOM focus, no `localStorage`.
- **Art is served, not inlined:** the webview only gets `http://127.0.0.1:<random>/<token>/<sha256(url)>-<size>.webp` from the session loopback server; never `file://`, never base64-inline a grid. `<img>` is only rendered once the file exists on disk (placeholder otherwise).
- **Controller input** goes through `gilrs` in Rust. WebKitGTK does expose `navigator.getGamepads()` (via `libmanette`) — do not use it: it only works while focused and can't be tested headlessly.
- **No `<video>`** in the v1 UI. It would bind the UI to GStreamer/WebKit and block the planned Blitz (`dioxus-native`) migration.
- **Launch composition is argv-only.** `LaunchSpec` is the single artifact shared by GUI, CLI and shim; never build shell strings or use `sh -c`.
- **Steam config writes** require Steam closed, atomic replace, a timestamped backup, and byte-identical restore. Round-trip tests for `localconfig.vdf` are mandatory. Reading it (playtime/last played) already lives in `stores::steam` and is side-effect free. The writer is `stores::localconfig` (format-preserving splice) + `stores::steam_config` (closed-guard, one restore point per app/user, journal + backup, atomic replace, exact-or-merged restore); `pitagames-cli launch-options` is the diagnostic. Details in `docs/m1-launch-options.md`.
- **One injection slot:** Tier A (Proton env), Tier B (manual OptiScaler), HelixSR and DLSSNR are mutually exclusive. Force `PROTON_USE_OPTISCALER=0` when DLSSNR owns `dxgi.dll`, and scan for conflicting files before install.
- **Never redistribute** DLSSNR model weights (`nvngx_dlssnr.dll` is NVIDIA's) or HelixSR built artifacts. Run upstream installers unmodified in a PTY and capture the launch-option lines they print.
- **DLSSNR scope:** `optiscaler` route only, x86_64 only. Backend comes from GPU architecture (RDNA 4 → upstream GitLab project; RDNA 3 → `mauri870/DLSSNR-RDNA3` fork, `experimental`); never offer one architecture's package to the other.
- **Console session:** `PITAGAMES_SESSION=1` must suppress the per-game gamescope wrapper. Nested gamescope is a bug.
- **Safe mode:** `--safe-mode` disables the DMABUF renderer, animations and blur; webview init failure must auto-retry into it. A black screen with no keyboard is unacceptable.

## Compatibility claims

- Only the RX 9070 and RX 5700 XT paths may ship as validated. Everything else (RDNA 3/2, NVIDIA, Intel) ships `experimental`/`unverified`, and the UI must say "not tested by us". No parity claims in docs or store copy.
- No TV is attached to either machine. 10-foot readability, overscan, focus return and the gamescope session ship marked "not tested on a TV" until community-validated.

## Build order

M0 vertical slice first: Steam VDF/ACF scan → library grid → `pitagames-shim` → launch one Steam game with `DXVK_HUD=fps`. Then M1 (runtime), M2 (other stores), M2.5 (console shell), M3 (graphics/upscalers), M3.5 (DLSSNR), M4 (hardening). Don't start store backends or mod orchestration before the M0 slice launches a real game.

**Current position:** M0 GUI/CLI shipped and container-verified; M1's install hand-off + Downloads board, the detail page (profiles + Steam playtime/last played), Steam `appdetails` metadata (rate limiter + SQLite cache + About block), the art pipeline (grid capsules + detail hero via a token-guarded loopback server), `platform::proton` discovery (read-only roots scan, kind/source priority, `ProtonRef` resolution, `pitagames-cli proton`), and the Steam launch-options writer (Steam-closed guard, atomic replace, timestamped backup, journal, byte-identical/merged restore, `pitagames-cli launch-options`) are done — see `docs/m1-metadata.md`, `docs/m1-art.md`, `docs/m1-proton.md`, `docs/m1-launch-options.md` and the progress log in the plan. Next task: the Proton picker + per-game override in the GUI (the first consumer of the writer), then the shim JSON profile read + `--explain`.
