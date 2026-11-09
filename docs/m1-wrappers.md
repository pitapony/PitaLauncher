# M1 — Wrapper-chain editor

Status: implemented and verified in `archlinux:latest` (GUI run with Xvfb).
Not yet exercised against a live Steam game on the RX 9070 / RX 5700 XT.

## Why this exists

A profile's wrapper chain is what turns this:

```text
pitagames-shim 620 %command%
```

into this at launch:

```text
gamescope -w 2560 -h 1440 -r 165 --fullscreen -- gamemoderun mangohud prism-run --prefer-wayland %command%
```

`platform::envc` already expanded `WrapperChain` into ordered `program + args`
steps (see `docs/m1-profile.md`), and the shim already executed them. The
missing piece was a way to edit that chain without hand-writing JSON. This
slice ships the detail-page **Wrappers** editor and makes one composition
function shared by the editor, the shim and `--explain`.

## Model (`core::profile::WrapperChain`)

```rust
pub struct WrapperChain {
    pub gamemode: bool,
    pub mangohud: bool,
    pub gamescope: Option<GamescopeOptions>,
    pub custom: Vec<WrapperStep>,     // { program, args }
}

pub struct GamescopeOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub refresh_hz: Option<u32>,
    pub fullscreen: bool,
    pub extra_args: Vec<String>,
}
```

The model is unchanged; the editor adds validation and helpers:

- `WrapperChain::is_empty()` — no process in front of the game.
- `WrapperChain::validate()` — rejects an empty custom program, NUL bytes in a
  program or argument, and a zero gamescope dimension (`width`/`height`/`refresh`).
  `Profile::validate` calls it, so a profile loaded from SQLite or imported from
  a file is held to the same rules as one typed in the UI.
- `GamescopeOptions::validate()` — the dimension/NUL checks above.

Validation is deliberately in `core`, not in the GUI: the compiler expands
wrappers into argv and the shim execs `argv[0]` directly, so an empty program
is a launch failure, not a cosmetic problem.

## Shared argv composition (`core::launch`)

The shim used to own `flatten` and `quote` privately, which meant the GUI could
only approximate what a launch would run. Those moved to `core::launch`:

| Helper | Purpose |
|---|---|
| `flatten_wrappers(wrappers, program, args)` | Outer-most-first flattening: `gamescope … gamemoderun … <game> …`. |
| `render_command(program, args)` | Shell-quoted copyable line for the transparency panel. |
| `shell_quote(token)` | POSIX single-quoting (`it's` → `'it'\''s'`). |

`pitagames-shim` now calls the same helpers, so the editor preview, the shim's
exec and `--explain` cannot drift apart. The launch path is still argv-only:
rendering a command string is for display, never for execution.

## The editor (`crates/app/src/wrappers.rs`)

`WrapperDraft` is the string form a text input can edit:

```rust
pub struct WrapperDraft {
    pub gamemode: bool,
    pub mangohud: bool,
    pub gamescope: bool,
    pub width: String,
    pub height: String,
    pub refresh_hz: String,
    pub fullscreen: bool,
    pub extra_args: String,
    pub custom: Vec<WrapperDraftStep>,   // { program, args } as typed
}
```

- `from_chain` loads a stored profile. Argument vectors are rendered back with
  `shell_quote`, so `--dls 'two words'` round-trips instead of losing the space.
- `to_chain` is the validation boundary: blank dimensions become `None`,
  non-numeric/zero dimensions are reported by field name, and extra/custom args
  are parsed with the same `tokenize_command` the shim uses for `%command%`
  (unbalanced quotes are reported, not guessed). A blank custom program is
  rejected by index.
- `add_step` validates a new row immediately, so the "Add step" button never
  appends something that would fail at save time.
- `apply_to(profile)` validates on a copy, so a refused draft never leaves the
  half-edited profile behind.
- `preview()` compiles through `envc::render_wrapper_chain`, i.e. the same
  ordering and gamescope suppression the launch uses.

The editor UI has:

- toggle chips for `gamemoderun`, `mangohud` and `gamescope`;
- gamescope width/height/refresh, fullscreen and extra-args fields when enabled,
  with the note that the console session (`PITAGAMES_SESSION=1`) suppresses the
  per-game gamescope step because it already owns a compositor;
- custom rows with Up/Down/Remove and an add row (`program` + `args`);
- a live **Preview** line, or an amber validation message instead.

Wrappers are saved with the rest of the profile by **Save & write profile**,
which writes both SQLite and `profiles/steam-<appid>.json`. Selecting another
profile ("Use") reloads the draft from that profile; deleting/creating a profile
reloads the list and the draft.

## Guarantees and limits

- **Guaranteed:** toggles, gamescope options and custom rows persist in the
  profile and appear in the shim file in launch order; the preview matches the
  shim's flattened command; invalid input is refused before it is saved; no
  shell string is executed; the console session still suppresses nested
  gamescope.
- **Custom rows are add/remove/reorder, not inline edit.** Change a row by
  removing and re-adding it.
- **Ordering is fixed for the named wrappers** (gamescope → gamemode →
  mangohud → custom), matching the plan's outermost-first rule. Custom rows can
  be reordered among themselves.
- **Availability is not checked yet.** `gamemoderun`, `mangohud`, `gamescope`
  and custom programs are only as available as they are on `PATH`; there is no
  pre-flight "not installed" warning. That belongs with the M3 wrapper work.
- **No live Steam game has run this chain.** The container smoke run proves the
  file and the flattened command, not that gamescope/gamemode/mangohud behave on
  the RX 9070 / RX 5700 XT.

## Verification

- `pitagames-core`: **51 tests** (was 46) — flattening, shell quoting, render,
  and wrapper/profile validation (empty program, NUL, zero dimension, empty
  chain).
- `pitagames-platform`: **29 tests** (was 28) — the preview matches the compiled
  chain and drops gamescope under the console session.
- `pitagames-shim`: **8 tests** (was 9; the quoting test moved to `core`) — it
  still flattens wrappers outermost-first through the shared helper.
- `crates/app`: **37 tests** (was 33) — draft round-trip, bad-input reporting,
  preview flattening/session suppression, and apply-without-mutation. Build,
  clippy `-D warnings` and the tests run in `archlinux:latest`.
- Xvfb smoke run with a fixture Steam root: toggled gamemode + mangohud +
  gamescope (2560×1440@165, fullscreen), added `prism-run --prefer-wayland
  "two words"`, confirmed the preview
  `gamescope -w 2560 -h 1440 -r 165 --fullscreen -- gamemoderun mangohud prism-run --prefer-wayland 'two words' %command%`,
  saved the profile, and read the shim file back with the four wrapper steps in
  order (`target/shot-wrapper-panel.png`, `shot-wrapper-gamescope.png`,
  `shot-wrapper-custom.png`, `shot-wrapper-save.png`). `pitagames-shim
  --explain 620 /bin/true` printed the identical flattened command.
- Other pure crates unchanged: db 17, art 14, metadata 22, stores 95 unit + 13
  integration, cli 18.

## Next

The owned-games Steam Web API key, then the single-instance lock, the
virtualized grid, the compat-DB loader and the diagnostics toggles/log viewer.
Graphics env (including the wrapper availability checks) is M3.
