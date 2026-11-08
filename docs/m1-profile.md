# M1 — Per-game runtime profile (shim profile + Proton picker)

Status: implemented and verified locally (pure crates) and in `archlinux:latest`
(`crates/app`). Not yet run against a live Steam client on the RX 9070 /
RX 5700 XT. The graphics half of the compiler is deliberately deferred to M3.

## Why this exists

Steam's launch options route a game through the shim, and Steam knows nothing
else about profiles:

```text
DXVK_HUD=fps %command% -novid
        ↓
DXVK_HUD=fps pitagames-shim 620 %command% -novid
```

The launch-options writer (see `docs/m1-launch-options.md`) puts the shim in
front of `%command%`. This slice gives the shim something to read: a small,
pre-compiled, per-game file the GUI writes on every profile save.

```text
~/.local/share/pitagames/profiles/steam-<appid>.json
```

The GUI compiles a `Profile` into it (`platform::envc`), the shim reads it and
applies the environment and wrapper chain, and the detail page gains a **Runtime**
panel: the Proton picker, the launch-options routing buttons and the current
state. This is the first consumer of the launch-options writer inside the GUI.

## On-disk contract (`core::profile_file`)

`ShimProfile` (`SHIM_PROFILE_SCHEMA = 1`) is written to
`profiles/<store>-<store_id>.json` — `steam-620.json` for Portal 2.

```json
{
  "schema": 1,
  "game": { "store": "steam", "store_id": "620" },
  "profile": "Default",
  "proton": {
    "reference": { "kind": "cachy_os" },
    "install": {
      "name": "proton-cachyos",
      "path": "/usr/share/steam/compatibilitytools.d/proton-cachyos",
      "kind": "proton-cachyos",
      "version": "proton-cachyos 10.0-20251101",
      "source": "system"
    }
  },
  "env": [["DXVK_HUD", "fps"], ["PROTON_LOG", "1"]],
  "wrappers": [{ "program": "gamemoderun", "args": [] }],
  "log_path": "/home/u/.local/share/pitagames/logs/shim.log",
  "written_at": "2026-11-07T18:04:05.123Z"
}
```

- `env` and `wrappers` are **ordered** and may repeat: later entries win,
  matching `LaunchSpec` semantics.
- `proton` is the choice recorded for the transparency UI. The shim does not
  apply it: for a Steam game Steam picks the runtime. Forcing a build in Steam
  itself is the `config.vdf` `CompatToolMapping` writer (see
  `docs/m1-compat-tool.md`), which the Runtime panel's "Force in Steam" button
  calls.
- Writes are **atomic** — sibling temp file, `fsync`, `rename` — exactly like
  `localconfig.vdf`. A half-written profile must never reach a launch.
- A **missing** profile is `Ok(None)`. A **corrupt** or unknown-schema file is an
  error the shim logs and then ignores; it is never fatal.

This is the only file I/O in `pitagames-core`. It lives here because the GUI, the
CLI and the shim must all agree on the bytes; the crate doc calls out the
exception.

## The compiler (`platform::envc`)

`compile_runtime(CompileRequest { profile, proton, log_dir, session })` returns a
`CompiledRuntime { env, wrappers, proton }`. It is a pure mapping — no I/O, no
clock, no environment reads (`session` is passed in) — so it is golden-testable.

M1 compiles the **runtime** subset:

| Profile field | Emitted |
|---|---|
| `diagnostics.dxvk_hud` | `DXVK_HUD=fps` |
| `diagnostics.mangohud` | `MANGOHUD=1` |
| `diagnostics.proton_log` | `PROTON_LOG=1` (+ `PROTON_LOG_DIR=<logs>`) |
| `diagnostics.fsr4_indicator` | `PROTON_FSR4_INDICATOR=1` |
| `diagnostics.dlss_indicator` | `PROTON_DLSS_INDICATOR=1` |
| `env` (advanced) | appended last, so a user override wins |
| `wrappers` | expanded to `program + args` steps |
| `proton` | resolved and recorded; no env (PROTONPATH is an umu/non-Steam concern) |

Wrappers are ordered outermost-first: gamescope, then gamemode, then mangohud,
then custom steps. `PITAGAMES_SESSION=1` suppresses the per-game gamescope step
because the console session already owns a compositor (nested gamescope is a
bug), matching plan §11.

**Graphics is refused, not dropped.** The plan's rule is that unsupported env is
rejected at compile time. M3 owns the FSR 4 / OptiScaler / HelixSR / DLSSNR env
table, so any profile that selects an injected upscaler, frame generation or
neural rendering fails with `EnvcError::GraphicsDeferred { feature }` naming the
field. The default profile (`Native` / `Auto` / `Off`) compiles to nothing and is
always valid.

## The shim (`pitagames-shim`)

```text
pitagames-shim <appid> <game command line>          # Steam's launch options
pitagames-shim --explain <appid> [command line]     # copyable explanation
```

1. Load `profiles/steam-<appid>.json`. A missing or corrupt file is a warning; the
   shim falls back to M0's `DXVK_HUD=fps` and still runs the game.
2. With a profile: export `PITAGAMES_APPID` and `PITAGAMES_PROFILE`, then apply the
   profile's `env` (later wins) and flatten its `wrappers` in front of the game.
3. Append one line to `<logs>/shim.log` (timestamp, `argv`, env) — best effort,
   never fatal.
4. `exec` the flattened command, forwarding the exit code and signals.

`--explain` prints the profile, the resolved Proton, the environment and the
flattened, shell-quoted command without running anything; it mirrors what the
GUI's Runtime panel shows and is what a bug report should paste.

Wrappers flatten as `w[0].program w[0].args… w[1].program w[1].args… <game>`, so
`gamescope -- gamemoderun mangohud <game>` is one argv. No shell is involved.

## The GUI (`crates/app`)

`crates/app/src/runtime.rs` is framework-free glue over `platform` and `stores`:
discover Proton, compile a profile, write its shim file, and read/write the Steam
launch options through `stores::steam_config`. The detail page's **Runtime**
panel:

- lists **Automatic**, **Steam default** and every discovered build (name + kind +
  source + version), with the automatic choice marked;
- **Save & write profile** stores the profile (with the chosen `ProtonRef`) in
  SQLite and writes its shim file, or reports why compilation was refused;
- shows Steam's process state and the current launch-options value;
- **Route through PitaGames** applies `pitagames-shim <appid> %command%` (the
  guarded writer: refuses while Steam runs, keeps a timestamped backup, replaces
  atomically) and **Restore original** puts the pre-PitaGames value back.

The profile list already persisted `ProtonRef`; the Runtime panel is where it is
now set and applied.

## CLI diagnostic

```bash
# Show the compiled profile the shim will read.
pitagames-cli profile 620
pitagames-cli profile 620 --json
pitagames-cli profile 620 --dir /path/to/profiles
```

## Guarantees and limits

- **Guaranteed:** a profile with no graphics settings compiles to a valid shim
  file; the file is written atomically; a missing/corrupt file never blocks a
  launch; the GUI's Apply/Restore use the same guarded writer as the CLI; the
  file's provenance (profile name, Proton choice, timestamp) is recorded.
- **Graphics is M3.** Injected upscalers, frame generation and neural rendering
  fail compilation by name until the graphics env table lands.
- **The Proton picker records a choice and can now force it in Steam.** The shim
  file carries the recorded build; the Runtime panel's "Force in Steam" /
  "Clear forced build" buttons write Steam's `config.vdf` `CompatToolMapping`
  through the same guarded writer (see `docs/m1-compat-tool.md`). Only
  `compatibilitytools.d` builds can be forced by name; Steam's own builds are left
  to Steam. Without a forced build, Steam still picks the runtime (usually
  whatever Steam's own setting says).
- **Wrappers apply from the shim, not from a launcher-managed process tree.**
  `gamemoderun`/`mangohud`/`gamescope` are only as available as they are on PATH.
- **The shim logs one launch line**, not a teed transcript: output tee-ing and
  process supervision are the rest of M1.
- **Hardware untested.** No live Steam client on the RX 9070 / RX 5700 XT has run
  this yet.

## Verification

- `pitagames-core`: **46 tests** (was 38), including 8 `profile_file` tests (path
  naming, JSON round trip, on-disk JSON shape, missing = `None`, corrupt JSON,
  unknown schema refused, no temp files left behind, parent directories created).
- `pitagames-platform`: **27 tests** (was 17), including 12 `envc` tests
  (default profile → empty, diagnostics env, log-dir omission, advanced-env
  ordering, wrapper ordering, `PITAGAMES_SESSION=1` suppression, Proton recorded
  but not turned into env, each deferred graphics field named, validation before
  compiling, `shim_proton` flattening).
- `pitagames-shim`: **9 tests** (was 3) — quoted command, profile env vs the M0
  fallback, empty command, tokenizer errors, wrapper flattening, direct run,
  shell quoting, Proton record rendering.
- `pitagames-stores`: **70 unit + 13 integration** — the `user_configs` resolver
  behind the GUI's routing panel.
- `pitagames-cli`: **14 tests** (unchanged) plus the new `profile` diagnostic
  command; it is exercised by the smoke run rather than unit tests.
- `crates/app`: **32 tests** (was 29), including three `runtime` tests that
  compile a profile and write the shim file, refuse a graphics profile without
  leaving a file, and drive apply → status → byte-identical restore through the
  real launch-options writer — all passing in `archlinux:latest` alongside
  `cargo build -p pitagames --all-targets` and
  `cargo clippy -p pitagames --all-targets -- -D warnings`.
- Smoke run (host): `pitagames-shim --explain 620` with a hand-written profile
  printed the environment and the flattened, quoted command; a run without a
  profile kept the M0 `DXVK_HUD=fps`; `pitagames-cli profile 620` printed the
  parsed file and its `--json` form; a missing profile produced a clear error.
- Docs: the shim/CLI/GUI behaviours above are mirrored in the CLI text, the
  `--explain` output and this document.

> The GUI was verified by build + clippy + tests in the container; no Xvfb
> screenshot was captured this session (the Docker VM was down to ~1.5 GB free
> and the `lld` link step needed headroom). A screenshot pass on the RX 9070 /
> RX 5700 XT is part of the same hardware checklist as the M0 closer.
