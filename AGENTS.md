# AGENTS.md

**PitaGames** — a Rust + Dioxus 0.7 game launcher for **CachyOS/Arch Linux**. Read this
file first: it names the invariants, the build/verify loop, and where each area is
documented. The design of record (architecture, domain model, env-compiler rules,
milestones, risks) lives at `/Users/julienpohie/.opencode/plan/pitagames.md` — the source
of truth when this file is silent.

## Agent skills

Issues and specs are GitHub issues in `pitapony/PitaLauncher`, driven with the `gh` CLI.
Five triage roles, each label string equal to its name: `needs-triage`, `needs-info`,
`ready-for-agent`, `ready-for-human`, `wontfix`. This file is the vocabulary source and
`docs/adr/` holds the decisions. See `docs/agents/issue-tracker.md`,
`docs/agents/triage-labels.md`, `docs/agents/domain.md`.

Non-trivial work runs through Matt Pocock's lifecycle skills — load the matching one by
ID before acting:

| Intent | Skill |
| --- | --- |
| brainstorm / grill a plan or decision | `grill-me`, `grill-with-docs` |
| write a spec / PRD | `to-spec` |
| break a spec into tickets | `to-tickets` |
| implement / TDD | `tdd`, then `implement` |
| bug / broken / slow / flaky | `diagnosing-bugs` |
| review a branch or PR | `code-review` |
| write a PR body | `pr` |
| compact a session | `handoff` |

## What this is

- A Rust + Dioxus 0.7 game launcher for **CachyOS/Arch Linux only**. Windows, macOS and
  Flatpak are explicit non-goals. License: MIT.
- Binaries `pitagames`, `pitagames-cli`, `pitagames-shim`; XDG dir `pitagames`. Never
  introduce `gamel` or `game-launcher-*` identifiers. The working directory is `pitagames`
  (renamed from `game-launcher` on 2026-11-07).

## Environment

- The dev host is macOS; the target is Linux (Arch/CachyOS). **The GUI cannot be built or
  run locally**: verify the pure crates on macOS and `crates/app` in an `archlinux:latest`
  container (or on the two target machines). Do not add macOS code paths to make local
  runs work.
- Target hardware is exactly two in-house machines: **RX 9070** (RDNA 4, 16 GB) and
  **RX 5700 XT** (RDNA 1, 8 GB). Everything else is `experimental`/`unverified`.
- cargo on this host needs `CARGO_HTTP_MULTIPLEXING=false` (libcurl stalls against the
  crates.io index; the sparse index then caches normally).

## Build & verify

**Pure-Rust crates, on macOS:**

```
cargo test -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata \
  -p pitagames-platform -p pitagames-stores -p pitagames-mods -p pitagames-repair \
  -p pitagames-secrets -p pitagames-input -p pitagames-shim -p pitagames-cli
```

plus `cargo clippy --all-targets` and `cargo fmt --all` over the same list. **Do not build
`crates/app` locally.**

**`crates/app` (the Dioxus crate) is built, linted and tested only in `archlinux:latest`**,
after installing `rust webkit2gtk-4.1 gtk3 libsecret sqlite pkgconf openssl xdotool`
(clippy/rustfmt ship inside Arch's `rust`; `libudev` arrives via `systemd-libs`; `xdotool`
supplies `libxdo`). For an Xvfb smoke run add `xorg-server-xvfb dbus mesa imagemagick`:
start `Xvfb :99`, run under `dbus-run-session`, drive with `xdotool`, screenshot with
`import -window root`. **Build into `/work/target-arch`** (`CARGO_TARGET_DIR=/work/target-arch`)
— the smoke scripts run `/work/target-arch/debug/pitagames`, so building without it leaves
them driving a stale binary. The container installs no status-notifier library, so only the
tray's failure path is covered there.

Two WebKit/Dioxus facts this project depends on: an **inline `style` can set CSS custom
properties**, but a **`<style>` element written in RSX is ignored** — a rule must travel
inline, never in a stylesheet.

MSRV is **1.89** (edition 2024 + let-chains; `std::fs::File::try_lock`). Keep `rust-version`
in `Cargo.toml` at 1.89 or higher.

## Crate map

| Crate | Role | Docs |
| --- | --- | --- |
| `pitagames-core` | Domain types + XDG paths; argv-only launch composition (`launch`), per-game profiles (`profile`, `profile_file`), design tokens (`design`), display/UI config (`ui`), i18n catalog (`i18n`), a11y (`a11y`), perf-budget model (`perf`), release/version helpers (`release`), console-session files (`session`), single-instance lock (`lock`), activation socket (`activation`), the `.desktop` entry renderer (`desktop`) | `docs/architecture.md`, `m1-profile.md`, `m1-wrappers.md`, `m2.5-design.md`, `m2.5-shells.md`, `m4-perf-budgets.md`, `v1.1-session.md`, `m1-single-instance.md`, `m1-desktop-integration.md` |
| `pitagames-db` | SQLite persistence: games / profiles / `game_metadata` / `owned_games`, the `launch_history` table, and the persisted install queue (migrations) | `docs/m2-launch-history.md`, `m4-crash-recovery.md` |
| `pitagames-stores` | Steam VDF/ACF scan + writers, and the M2 backends (Epic/Amazon/GOG) on one shared PTY CLI driver; importers; install queue + installer | `docs/m2-cli-backend.md`, `m2-epic.md`, `m2-amazon.md`, `m2-gog.md`, `m2-importers.md`, `m2-store-sync.md`, `m2-store-installs.md`, `m2-cancel-install.md`, `m1-launch-options.md`, `m1-compat-tool.md` |
| `pitagames-metadata` | Steam `appdetails` fetch + `IPlayerService/GetOwnedGames`, rate limiting, cache policy | `docs/m1-metadata.md`, `m1-owned-games.md` |
| `pitagames-platform` | Host detection + compilation: Proton, GPU, compat DB, capability matrix, `envc`, prefixes/disk, launch hand-off, NR, EAC, power, logs, verify/support | `docs/m1-proton.md`, `m3-gpu.md`, `m3-capability.md`, `m3-tiera.md`, `m1-compat-db.md`, `dlssnr.md`, `m3.5-nr-*.md`, `eac-v2.md`, `m1-anticheat.md`, `m2-launch.md`, `m2-prefixes.md`, `v1.1-power.md`, `m1-diagnostics.md` |
| `pitagames-mods` | Mod primitives: manifest + backup/restore, release fetch + SHA-256 verify, INI patch, launch-option capture, Tier B deploy, conflict scan | `docs/mods-manifest.md`, `mods-primitives.md`, `m3-tierb.md`, `m3-injection-slot.md` |
| `pitagames-repair` | Repair installation and the local settings export | `docs/m4-repair.md` |
| `pitagames-secrets` | The Steam Web API key chain (env → keyring → `0600` file) | `docs/m1-owned-games.md` |
| `pitagames-input` | Controller + keyboard input: `Input`, bindings/`InputMap`, focus graph, ragged layout, on-screen keyboard, remap session, pad-permission probe, glyph hints | `docs/m2.5-input.md`, `m2.5-focus.md`, `m2.5-remap.md`, `m2.5-osk.md`, `m2.5-pad-permission.md`, `m2.5-hints.md` |
| `pitagames-art` | Art download → cover-crop → 2× WebP cache → token-guarded loopback server | `docs/m1-art.md` |
| `pitagames-shim` | The Steam launch-options shim: reads `profiles/steam-<appid>.json`, flattens wrappers, `--explain` | `docs/m1-profile.md` |
| `pitagames-cli` | The headless diagnostic/ops CLI (`pitagames-cli …`) | see below |
| `pitagames-app` | The Dioxus GUI — **the only crate allowed to touch Dioxus/RSX** | `docs/console-mode.md`, `m2.5-home.md`, `m2.5-overlay.md`, `m2.5-safe-mode.md`, `m3-graphics.md`, `m3-settings.md`, `m2.5-console-library.md` |

`pitagames-cli` subcommands: `scan`, `install`, `metadata`, `proton`, `gpu`, `capability`,
`nr`, `support {verify,bundle}`, `compat`, `anticheat`, `eac`, `profile`, `logs`, `import`,
`prefix`, `disk`, `perf`, `mods {release,ini,capture,…}`, `repair`, `history`, `launch`,
`stores {status,updates,sync,update}`, `desktop-entry`, `session`, `activate`,
`launch-options`, `compat-tool`, `steam-account`, `epic`, `amazon`, `gog`.

## Non-negotiable constraints

- **Renderer boundary:** only `crates/app` may use Dioxus/RSX. `core`, `db`, `stores`,
  `art`, `platform`, `mods`, `metadata`, `input`, `shim`, `repair`, `cli` stay
  framework-free. No JS navigation, no DOM focus, no `localStorage`.
- **Controller input goes through `gilrs` in Rust.** WebKitGTK exposes
  `navigator.getGamepads()` — do not use it (focused-only, untestable headlessly).
- **Art is served, not inlined:** the webview only ever gets
  `http://127.0.0.1:<random>/<token>/<sha256(url)>-<size>.webp` from the session loopback
  server — never `file://`, never a base64-inlined grid. An `<img>` renders only once the
  file exists on disk (placeholder otherwise).
- **No `<video>`** in the v1 UI — it binds the UI to GStreamer/WebKit and blocks the
  planned Blitz (`dioxus-native`) migration.
- **Launch composition is argv-only.** `LaunchSpec` is the single artifact shared by GUI,
  CLI and shim; never build shell strings or use `sh -c`. The shim's per-game config is the
  `ShimProfile` JSON (`core::profile_file`, written atomically) and the shim applies its env
  and wrapper chain as vectors too.
- **Steam config writes** require Steam closed, atomic replace, a timestamped backup, and
  byte-identical restore; round-trip tests for `localconfig.vdf` are mandatory. Reading it
  (playtime/last-played) lives in `stores::steam` and is side-effect free. The writers are
  `stores::vdf_edit` (shared span splicing), `stores::localconfig`/`compat_tool`
  (format-preserving edits) and `stores::steam_config` (closed-guard, one restore point,
  journal + backup, atomic replace, exact-or-merged restore).
- **Secrets never leak by accident:** the only secret is the Steam Web API key, held as
  `pitagames_secrets::SteamApiKey` (no `Display`, redacting `Debug`, raw value behind
  `expose_secret`). Never log, print or JSON-serialize a key; the UI shows `masked()` only.
  Every API error is scrubbed of it.
- **One injection slot:** Tier A (Proton env), Tier B (manual OptiScaler), HelixSR and
  DLSSNR are mutually exclusive. Force `PROTON_USE_OPTISCALER=0` when DLSSNR owns
  `dxgi.dll`, and scan for conflicting files before install.
- **One mod manifest for every mod:** Tier B OptiScaler, HelixSR and DLSSNR all write
  `crates/mods`' `ModManifest` (`mods::manifest`) — every file created, every original moved
  to `<game>/.gl-backup/`, a BLAKE3 hash per file, a pre-install tree hash. Install
  validates paths before touching the tree; uninstall (`restore`) verifies the backups first
  and refuses a tampered one rather than clobbering, then reports whether the tree hash
  returned. `blake3` is the mod/tree hash; `sha2` stays for release and Steam-config digests.
- **Never redistribute** DLSSNR model weights (`nvngx_dlssnr.dll` is NVIDIA's) or HelixSR
  built artifacts. Run upstream installers unmodified in a PTY and capture the launch-option
  lines they print.
- **DLSSNR scope:** `optiscaler` route only, x86_64 only. The backend comes from GPU
  architecture (RDNA 4 → the upstream GitLab project; RDNA 3 → `mauri870/DLSSNR-RDNA3`,
  `experimental`); never offer one architecture's package to the other.
- **Console session:** `PITAGAMES_SESSION=1` must suppress the per-game gamescope wrapper —
  nested gamescope is a bug.
- **Safe mode:** `--safe-mode` disables the DMABUF renderer, animations and blur, and a
  webview init failure must auto-retry into it. A black screen with no keyboard is
  unacceptable.
- **Updates ride the install queue** (ADR-0030); the store CLIs (`legendary`, `nile`,
  `lgogdownloader`, `gogdl`) are the package manager's job and are never self-updated.

## Compatibility claims

- Only the **RX 9070** and **RX 5700 XT** paths may ship as validated. Everything else
  (RDNA 3/2, NVIDIA, Intel) ships `experimental`/`unverified`, and the UI must say "not
  tested by us". No parity claims in docs or store copy. (ADR-0004)
- No TV is attached to either machine. 10-foot readability, overscan, focus return and the
  gamescope session ship marked **"not tested on a TV"** until community-validated.

## Status

Implemented and container-verified, all merged to `main`:

- **M0–M2** — the Steam scan/writers/metadata/art, the shim profile, the three store
  backends on one PTY CLI driver, the importers, the install queue with GUI installs and
  cancellation, the umu/Proton and store-client launch hand-off, and the launch history.
- **M2.5** — the console shell (input layer, focus graph, gamepad pump, remap, on-screen
  keyboard, home, overlay, launch hand-off, safe mode, the 10-foot design pass, the test
  backbone) and the console library + game page.
- **M3** — GPU/capability, the Graphics and Settings screens, Tier A/B injection, and the
  single injection slot + conflict scan + anti-cheat gate.
- **M3.5** — the DLSSNR backends, installer, verify/support bundle and uninstall.
- **M4** — updates on the install queue, repair + settings export, crash recovery,
  performance budgets, i18n, accessibility, release packaging and beta artifacts.
- **v1.1** — the console session (gamescope + a systemd user unit) and logind power rows.

**Not hardware-validated** (ADR-0004): the M0 closer (`docs/M0-test.md` — one real Steam
game with `DXVK_HUD=fps`), the graphics-validation paths, and the NR flow. No store backend
has been driven against a real `legendary`/`nile`/`lgogdownloader`/`gogdl` yet; each slice's
doc carries its own "not yet run against X" caveats.

**Next:** the M0 hardware closer — first in line as soon as either target machine is
reachable. Release tagging, signatures, the AUR packages and the CachyOS repo proposal are
M4 follow-ups.

## Where things are documented

- `docs/architecture.md` — the system overview.
- `docs/user-guide.md`, `docs/console-mode.md`, `docs/troubleshooting.md` — end-user guide,
  the couch reference, and troubleshooting.
- `docs/beta-testing.md`, `docs/compat-contribution.md` — contributor checklists.
- `docs/adr/` — the architecture decision records (`ADR-NNNN`).
- `docs/agents/` — the workflow config (issue tracker, triage labels, domain).
- One doc per shipped slice, named `docs/m<slice>-<feature>.md` (`m1-*`, `m2-*`, `m2.5-*`,
  `m3-*`, `m3.5-*`, `m4-*`, `v1.1-*`), plus `docs/dlssnr.md`, `docs/eac-v2.md`,
  `docs/mods-*.md` and `docs/graphics-validation.md`. Each crate-map pointer above names the
  docs for its area.
