# M3 — the Settings screen (`View::Settings`)

Status: implemented and unit-tested (pure model) plus an Xvfb run at 1080p in
both shells. Every control reads and writes the **same files** the rest of the
launcher does; nothing here is a hardware claim (ADR-0004).

## Why this slice exists

The plan's screen table promises a Settings screen that holds the launcher's
preferences in one place — "Proton/runtime paths, wrapper defaults, Steam config
backup/restore, compat DB sync, theme, start mode / display". Until this slice
the preferences only existed as a hand-edited `config.toml`; the safe-mode doc
even said "there is no Settings screen yet, so the file is the toggle".

Three rules shape it:

- **One source per value.** The screen loads and saves `config.toml` through the
  same `pitagames_core::ui::AppConfig` the shell, the supervisor, the session
  entry and the CLI's `session` command read. It owns no second store.
- **Compose, do not duplicate.** The compat sync is `platform::compat`, the
  Steam restore is `runtime::restore`, the wrapper default is the profile's own
  `WrapperChain`, the shell switch is the shell state's `set`, and remapping is
  a **link** to the Controls screen rather than a second editor.
- **Reachable without a pointer.** Every control is a cell in one
  `FocusGraph::ragged`, drained from the same `ActionChannel` as the other
  screens, so the keyboard and a pad drive it.

## Pieces

| Item | Role |
|---|---|
| `app::settings` (`crates/app/src/settings.rs`) | the framework-free model (`Control`, `Intent`, `rows`/`cells`/`bands`/`graph`, `selected`, `toggle_on`, `apply`) and the one Dioxus component |
| `app::runtime` (`runtime::new_profile`) | builds a new profile, seeding its wrapper chain from the Settings default |
| `app::detail` (`create_profile`) | creates profiles through `runtime::new_profile` |
| `app::shell` (`ShellState::set`) | the theme chips switch the live shell and remember it, exactly as `F11`/`Guide` do |
| `core::ui` (`AppConfig`, `CompatConfig`, `[wrapper]`) | the file, the compat-sync preference and the default wrapper chain |
| `platform::compat` (`sync_shipped`/`sync_sources`) | the offline compat-DB copy |
| `app::pumps` (`bootstrap`) | refreshes the synced copy once per launch when the toggle is on |
| `input` (`SETTINGS_HINTS`) | the cheat-sheet row (Change, Back) |
| `app::navigation` | `View::Settings` and its place in `DESTINATIONS` (so `Tab`/`E`/`Q` reach it) |

## The model

`Control` is the closed set of focusable cells; `rows()` groups them into the
visual lines the screen draws and is the single source of both `cells()` (the
flat cell ids) and `bands()` (one `Band` per row). `graph()` is
`FocusGraph::ragged(&bands())`, so the d-pad and the arrow keys walk the same
layout the screen renders and no control is stranded (a property test walks every
cell to every other).

`apply(control, &mut AppConfig) -> Intent` is the whole behaviour: it mutates the
one field the control names and returns what the component must do next —
`Save`, `SaveAndSync` (the compat toggle turning *on* copies the database now),
`Sync` (the **Sync now** button), `SwitchShell` (the live shell is the shell
state's business, not a config write), `RestoreSteam`, or `OpenControls`. The
component re-reads the file before applying, so it can never clobber a field
another writer (the shell's remembered `last_shell`) changed while it was open.

## Controls

| Section | Control | Writes |
|---|---|---|
| Theme | Desktop / Console | the live shell (and `[ui] last_shell`) |
| Start mode | Desktop / Console / Remember | `[ui] start_mode` |
| Display | UI scale −/+ | `[ui.display] ui_scale` (clamped) |
| Display | Overscan margin −/+ | `[ui.display] safe_area_pct` (clamped) |
| Display | Cursor hide delay −/+ | `[ui.display] hide_cursor_ms` (cycles 0/1.5/3/5 s) |
| Behaviour | Reduce motion | `[ui.display] reduce_animations` |
| Behaviour | Console fullscreen | `[ui.display] console_fullscreen` |
| Behaviour | **Safe mode** | `[ui] safe_mode` |
| Wrapper defaults | GameMode / MangoHud / Gamescope | `[wrapper]` |
| Compatibility database | **Keep a synced copy** | `[compat] sync` (and copies now) |
| Compatibility database | Sync now | copies `compat/*.toml` into the data directory |
| Steam configuration | Restore Steam changes | `runtime::restore` for every Steam row |
| Controls | Remap input | leaves to `View::Controls` |

The **safe-mode** and **compat-sync** toggles are the two the ticket names first;
both are plain `[ui]`/`[compat]` booleans the rest of the launcher already reads.

## Wrapper defaults

`[wrapper]` in `config.toml` is the exact `WrapperChain` a profile stores. A
profile created from the detail page is built by `runtime::new_profile`, which
copies that chain in (after validating it — a chain a hand edit broke is ignored
rather than blocking the profile). The game's own Runtime panel overrides it, and
the console session still drops the per-game Gamescope step under
`PITAGAMES_SESSION=1`, so the default cannot nest a compositor.

## The compat-DB sync

`platform::compat::sync_shipped` copies the packaged `*.toml` files into
`$XDG_DATA_HOME/pitagames/compat`, the location `default_shipped_dirs` already
reads beside the package. The copy is idempotent (identical bytes are counted,
never rewritten) and `sync_sources` never lets the copy be its own source, so a
stale copy cannot win the loader's later-source merge. The toggle copies when it
is switched on, **Sync now** copies on demand, and `pumps::bootstrap` refreshes
the copy once per launch while the preference is on — so a package update is
picked up without opening Settings.

## Reading the rest

- The **runtime paths** (discovered Proton builds, the Steam root, the prefixes,
  games and logs directories) are read-only `PathLine`s; nothing is created.
- **Restore Steam changes** runs the same guarded writer the detail page uses,
  once per Steam row, and reports how many were restored and the first error. It
  refuses while Steam is running, which is the writer's own rule.
- The **Controls** button navigates to `View::Controls`; remapping has exactly
  one editor.

## Known limits

- The monitor picker (`[ui.display] monitor`) is not on the screen yet: monitor
  handles only exist after the window is built, and 4K/HiDPI placement is the
  deferred pass (`docs/m2.5-shells.md`).
- The wrapper default is a starting point only; there is no per-store default.
- The runtime paths are informational — the CLI's `prefix`/`proton`/`disk`
  commands are still where a path override is set.
