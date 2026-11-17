# M2 — the per-game runtime editor for non-Steam rows

Status: implemented and container-verified. The Profiles panel and the Runtime
panel (Proton picker, wrapper chain, diagnostics) now render for every store,
and the Command panel previews the *actual* launch — `umu-run <exe>`, a native
executable, or the store's client — instead of the Steam shim line.
**Not yet run against a real store game** (no machine has umu plus a real
Epic/Amazon/GOG install yet); the compose path is covered by tests and an Xvfb
smoke run.

## Why this slice exists

The previous two launch slices made a non-Steam row playable through the
umu/Proton hand-off, but the row still had **no profile editor**: the detail
page's Profiles and Runtime panels were wrapped in `if is_steam`, and a
non-Steam game launched with whatever a `profiles/<store>-<id>.json` held —
which only `pitagames-cli profile` could write. The file shape, the compiler
(`platform::envc`), the DB-backed profile list and the SQLite profile store were
already store-generic; only the GUI gate was Steam-shaped.

This slice removes the gate and gives the panel a non-Steam preview:

```text
  detail page (any store)
        │
   Profiles panel ───────────────► SQLite profiles (already keyed by GameId)
        │
   Runtime panel
        ├─ Proton picker ─┐
        ├─ wrapper chain  ├─► Profile draft ─► platform::envc
        └─ diagnostics  ──┘                        │
                                                   ▼
        Steam  →  pitagames-shim <appid> %command%   (unchanged)
        other  →  runtime::compose_handoff ─► platform::launch::prepare
                                                   │
                                          umu-run / native / store client
        │
   Save & write profile ─► profiles/<store>-<id>.json (the shim/hand-off reads it)
```

## One composition, two callers

`crates/app/src/runtime.rs` gains the composition that used to live inline in
`play::start_game`:

```rust
pub fn compose_handoff(
    game: &Game,
    runtime: Option<&ShimProfile>,
    report: &DiscoveryReport,
) -> Result<Prepared, String>;

pub fn handoff_preview(
    game: &Game,
    profile: &Profile,
    report: &DiscoveryReport,
) -> Result<HandoffPreview, String>;

pub fn client_launch(game: &Game) -> Result<Option<LaunchSpec>, String>;
```

`compose_handoff` is now the single seam between a `Game` row and a prepared
`LaunchSpec`:

- it resolves the Proton build the profile recorded (else discovery), and is
  **lenient about a missing build for a native row** — a machine with no Proton
  must still start a Linux game, which the first Xvfb run of the previous slice
  caught;
- it builds the store-client argv when the row names no executable
  (`client_launch`);
- it calls `platform::launch::prepare` with the session flag.

`play::start_game` (the Play button's worker) and `runtime::handoff_preview`
(the Runtime panel) both call it, so the line the panel shows and the process a
Play starts **cannot drift**. `handoff_preview` compiles the *draft* profile
(unsaved edits included) with `platform::envc` and renders it through the same
path; it writes nothing.

## The detail page

| Panel | Before | Now |
|---|---|---|
| Profiles | `if is_steam`, else "still Steam-only" | every store; the note is gone |
| Runtime — Proton picker | Steam only | every store (reported as `PROTONPATH` for umu) |
| Runtime — wrappers/diagnostics | Steam only | every store |
| Runtime — routing/compat buttons | Steam only | still Steam only (`is_steam`-filtered) |
| Command panel | `pitagames-shim <appid> …` | Steam: the shim line; other: the hand-off line + the launch shape (mode · prefix) |
| Save & write profile | wrote `profiles/<store>-<id>.json` | unchanged; now reachable for every store |

The Steam routing/compat block stays gated: `route_root`/`routing_status` are
filtered with `is_steam`, so a non-Steam game never offers "Route through
PitaGames" or "Force in Steam" even when Steam is installed on the machine. The
command sub-label also branches ("what pitagames-shim will run" vs "what the
launch hand-off will run").

For a **store-client** row (GOG's `gogdl`, Amazon's `nile`) the Command panel
shows the client's argv and `launch shape: client · no per-game prefix`: the
store client owns its own Wine/Bottles runtime, so the per-game prefix the umu
path uses does not apply — the panel says so rather than implying one.

## Verification

```bash
# pure crates (macOS)
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata \
  -p pitagames-platform -p pitagames-stores -p pitagames-secrets \
  -p pitagames-shim -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check

# GUI (archlinux:latest, CARGO_TARGET_DIR=/target)
cargo test -p pitagames
cargo clippy -p pitagames --all-targets -- -D warnings
```

- `runtime` tests (5 new, moved 4 from `play`): a Windows row previews the umu
  hand-off (`umu`, a prefix ending in `epic-Gizmo`, `DXVK_HUD`, `GAMEID`,
  `STORE`); a native row previews a direct launch **with no Proton discovered**;
  a store-client row previews its `gogdl` argv with no prefix; a not-installed
  row reports the hand-off refusal; a graphics profile is refused by name.
  The composition helpers moved with their tests (`only_a_windows_build_needs_proton`,
  the three `client_launch` cases).
- `play` keeps its six slot/supervision tests; the four launch-composition tests
  moved to `runtime` with the functions.
- `crates/app` is **104 tests** (was 99). `cargo fmt --all --check`, the pure
  suite and clippy are clean on macOS, and `crates/app` builds, lints and tests
  in `archlinux:latest`.
- **Xvfb smoke run** (one seeded Epic row with a `.exe`, a fake `proton-cachyos`
  tree and a fake `umu-run` on `PATH`): the grid showed the Epic card with
  **Play**; the detail page rendered the **Profiles** and **Runtime** panels with
  the non-Steam copy; creating a profile selected it; the **Command** panel read

  ```text
  GAMEID=epic-Gizmo PROTONPATH=/usr/share/steam/compatibilitytools.d/proton-cachyos \
  WINEPREFIX=…/prefixes/epic-Gizmo STORE=epic umu-run …/Gizmo.exe
  launch shape: umu · prefix …/prefixes/epic-Gizmo
  ```

  and **Save & write profile** reported `Wrote …/profiles/epic-Gizmo.json` and
  left a valid file (schema 1, the resolved Proton recorded, empty env/wrappers).
  No panic in the log.

## Known limits

- **Not driven against a real store game.** The argv and the preview are the
  tested composition; no machine has umu plus a real Epic/Amazon install.
- **The Steam-only graphics half** (upscalers, frame gen, injection) is still
  refused by `platform::envc` for every store until M3, so the Runtime panel's
  command preview reports the refusal by name rather than a partial command.
- **A not-installed row cannot preview its command** — `platform::launch::prepare`
  refuses `NotInstalled` before composing, so the Command panel shows that
  sentence. A profile can still be written for such a row (useful before an
  install), and the editorial panels work.
- **A store client launch stays a black box**: the profile's environment applies,
  but `gogdl`/`nile` pick their own runtime, so the per-game prefix is not used.
- The routing/compat buttons and the `config.vdf` writer remain Steam-only by
  design (they write Steam's files).
