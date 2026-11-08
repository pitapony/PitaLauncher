# M1 — Steam compatibility-tool writer (`config.vdf` `CompatToolMapping`)

Status: implemented and verified against fixtures + CLI smoke runs. The GUI
Runtime panel consumes it ("Force in Steam").
**Not yet exercised against a live Steam client on the RX 9070 / RX 5700 XT.**

## Why this exists

Choosing a Proton build in PitaGames is only half the story. The shim reads the
build from `profiles/steam-<appid>.json`, but for Steam to actually pass a game
through that build end to end — `PROTONPATH`, the compatibility-tools search
path, the per-game prefix Steam already created — the choice has to live where
Steam looks:

```text
Steam → Libraries → <game> → Properties → Compatibility
        → "Force the use of a specific Steam Play compatibility tool"
```

Steam stores that as a `CompatToolMapping` entry in `<steam>/config/config.vdf`:

```text
"InstallConfigStore" {
  "Software" { "Valve" { "Steam" { "CompatToolMapping" {
    "620" {
      "name"     "proton_9"
      "config"   ""
      "Priority" "250"
    }
  } } } }
}
```

Forcing `proton-cachyos` or a `GE-Proton` build here is what makes the GUI's
Proton picker take effect in Steam itself; without it the pick only affects the
shim's own env. `config.vdf` is Steam's file (it also holds `cip`,
`AutoUpdateWindowEnabled`, `last_changelist` and much more), so the writer is a
**byte-level splice** under the same guard rails as the launch-options writer.

## Modules

| Layer | File | Responsibility |
|---|---|---|
| Span editing (shared) | `crates/stores/src/vdf_edit.rs` | Order-preserving, span-aware VDF scanner and the `insert_pair` / `insert_object` / `remove_pair` splices both writers use. |
| Launch options | `crates/stores/src/localconfig.rs` | `localconfig.vdf` `LaunchOptions` read/edit on top of `vdf_edit`. |
| Compatibility tool | `crates/stores/src/compat_tool.rs` | `config.vdf` `CompatToolMapping` read/edit (`CompatTool { name, config, priority }`). |
| Guarded write/restore | `crates/stores/src/steam_config.rs` | Steam-closed check, backup, restore journal, atomic replace, exact-or-merged restore, shared by both writers. |
| Tool-name mapping | `crates/platform/src/proton.rs` | `ProtonInstall::compat_tool_name()` — the Steam name for a discovered build. |
| CLI diagnostic | `crates/cli/src/main.rs` | `pitagames-cli compat-tool …`. |

All are framework-free and DB-unaware. The GUI calls the same functions through
`crates/app/src/runtime.rs`.

## Format-preserving edit

`compat_tool::read(input, app_id)` walks
`InstallConfigStore → Software → Valve → Steam → CompatToolMapping → <appid>`
and returns a `CompatTool`. A missing section, missing app or a mapping without a
`name` is `Ok(None)`; only the writer insists on the expected structure.

`compat_tool::edit(input, app_id, Some(tool))`:

- parses the whole document with the public VDF parser first (a broken file is
  never edited);
- inserts a whole `name`/`config`/`Priority` object in one splice when the app
  has no mapping;
- when a mapping exists, replaces each pair's quoted value (or inserts the pair
  when it is missing), re-scanning between splices because every splice shifts
  the offsets after it;
- re-parses the result before returning it.

`None` removes the mapping. An object that only ever held compat pairs goes away
with them, so a set-then-clear round trip returns the original layout; an object
that carries unrelated keys loses only the compat pairs. Indentation is copied
from the surrounding object. Comments and `[$CONDITIONAL]` suffixes survive
because no other byte is touched.

Fixtures live in `fixtures/steam/`:

- `config_empty.vdf` — an empty `CompatToolMapping` plus unrelated keys;
- `config_with_compat.vdf` — an existing `proton_9` mapping.

## Guarded write

`steam_config::apply_compat_tool(config, app_id, tool, steam_state, options)`
runs through the same machinery as `apply_launch_options`
(`steam_config::apply_edit`):

1. **Refuse while Steam runs** (`detect_steam_process`; non-Linux is `Unknown`
   and needs an explicit override).
2. **Read and edit without writing.** The whole file must be UTF-8.
3. **No-op if the mapping already matches.** Nothing is backed up.
4. **One restore point per app and config file** (`config-global-<app>.json` next
   to the backup; `config.vdf` is install-wide, so its scope is `global`). The
   launch-options records use the `localconfig` stem, so the two writers never
   collide.
5. **Journal before the replace**, atomic replace, directory `fsync`, original
   file mode preserved.
6. Return `CompatWriteReport { previous, applied, changed, backup }` (typed
   `CompatTool` values).

Backups default to `$XDG_DATA_HOME/pitagames/backups/steam` and are named
`config-global-<app>-<unix-nanos>-<pid>.vdf`.

`restore_compat_tool` mirrors `restore_launch_options`: a byte-identical file
restore when the file still matches what PitaGames wrote, otherwise a merge that
splices only this app's mapping back (keeping a `…-pre-restore.vdf` copy) so
Steam's other changes are never clobbered.

## Which build can be forced

`ProtonInstall::compat_tool_name()` derives Steam's name without guessing:

| Build found in | Steam name | Forcible |
|---|---|---|
| system / user `compatibilitytools.d` | the folder name (`proton-cachyos`, `GE-Proton9-20`) | yes |
| Steam's own `steamapps/common` (`Proton 9.0`, `Proton - Experimental`) | internal (`proton_9`, `proton_experimental`) | **no** — Steam's own UI owns those |
| umu's private folder, an explicitly picked path | not in Steam's search path | **no** |

The GUI enables "Force in Steam" only for a choice with a `tool_name`; Steam's
own builds are left to Steam. The CLI takes a literal `--set <name>`, so a user
who knows the internal name can still pass it.

## CLI

```bash
# Read the current mapping (also prints the resolved root and Steam state).
pitagames-cli compat-tool 620

# Force a compatibility tool (optionally with a priority).
pitagames-cli compat-tool 620 --set proton-cachyos
pitagames-cli compat-tool 620 --set GE-Proton9-20 --priority 250

# Clear it, so Steam decides again.
pitagames-cli compat-tool 620 --clear

# Put the original mapping back.
pitagames-cli compat-tool 620 --restore

# Fixture/other-install diagnostics.
pitagames-cli compat-tool 620 --config /path/to/config.vdf --force
```

Target resolution: `--config <path>` edits that file directly and derives the
Steam root when the file is `<root>/config/config.vdf`; otherwise `--steam-root`
(or auto-detection) resolves `<root>/config/config.vdf`. `--force` is required
when Steam is running or the platform cannot inspect processes (macOS
development runs). `--json` prints the same fields as the text output.

## Guarantees and limits

- **Guaranteed:** only the target app's mapping changes; the file is valid VDF
  after every edit; writes are atomic; a backup always exists before the replace;
  an unchanged file restores byte-for-byte; a repeat write refuses if the
  original restore-point backup disappeared or changed.
- **One restore point per app and config file**, the value from before the first
  PitaGames write. A journal from a different Steam root is refused.
- **Merged restores cannot be byte-identical** when Steam changed other keys in
  between; they preserve Steam's changes and restore only the mapping.
- **Steam detection is Linux-only**; macOS requires `--force`.
- **Only `compatibilitytools.d` builds are named.** Valve's internal tool names
  and builds outside Steam's search path are not invented; the CLI's `--set`
  remains the escape hatch.
- **Hardware untested.** Fixtures and CLI smoke runs prove the byte behaviour; a
  real Steam client on the RX 9070 / RX 5700 XT has not run it yet.

## Verification

- `pitagames-stores`: 16 `compat_tool` tests (read/defaults/insert/replace/
  set-then-clear round trip/clear/refusals/inline/comments/unrelated keys) and
  9 new `steam_config` tests for the compat writer (byte-identical set-then-
  restore, replace mapping, clear-then-restore, repeated writes, running-Steam
  refusal, merged restore, no-op, no record, no record collision with launch
  options). The shared-journal refactor kept all 21 launch-options tests green.
- The `vdf_edit` extraction from `localconfig.rs` is behaviour-preserving: the 14
  `localconfig` tests and the launch-options round-trip tests are unchanged.
- `pitagames-platform`: `compat_tool_name` test (system/user tools named, Valve/
  umu/explicit builds not). Platform is now 28 tests.
- `pitagames-cli`: 4 new tests for argument parsing, action conflicts,
  `<root>/config/config.vdf` derivation and the planned mapping; CLI is now 18
  tests.
- CLI smoke run on a fixture tree: read → `--set --dry-run` → `--set` → read →
  `--json` → `--restore`; the restored file compares byte-identical with the
  original and the backup directory holds one `.vdf` plus the `.json` journal
  (removed on restore).
- `crates/app` builds, passes clippy `-D warnings` and its 33 tests in
  `archlinux:latest`; the Runtime panel gains "Force in Steam" / "Clear forced
  build" / "Restore forced build" and shows the current forced tool.
