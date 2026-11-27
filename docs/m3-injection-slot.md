# M3 — the injection slot, the conflict scan and the anti-cheat gate

Four mechanisms can put code into a game's process: Proton's own env hooks
(**Tier A**), the launcher's managed OptiScaler files (**Tier B**), **HelixSR**
and **DLSSNR** (both of which ship their own OptiScaler). They share one slot,
so they are mutually exclusive (ADR-0023). This slice makes the rule one
mechanism instead of three opinions, and adds the pre-flight that names what is
already in a game folder.

## One slot, one vocabulary

`pitagames_core::profile::SlotOwner` (`None | TierA | TierB | HelixSr | DlssNr`)
is the single name for the slot, and `Profile::slot_owners()` folds the fields
that can own it (the `injection` choice, the upscaler, frame generation and
neural rendering) into that vocabulary. The graphics screen's "who owns the
slot" answer and the compiler's conflict cannot disagree because both read the
same function:

- `Profile::validate()` still refuses the combinations it enumerates.
- `platform::envc`'s `slot_conflict` returns the first two owners as a typed
  `EnvcError::SlotConflict { left, right, resolve }` — a resolve path, never
  contradictory env. It is driven by `Profile::slot_owners()`.
- `platform::capability` hides the other owner through `availability()`
  (`NR_OWNS_SLOT` / `UPSCALER_OWNS_SLOT`), so the UI never offers two.

## Switching records and is reversible; the pre-flight names owners

`pitagames_mods::conflict` is the read-only pre-flight: it resolves the game's
executable directory (an Unreal `*/Binaries/Win64` layout included, the same
resolution a Tier B deploy uses) and matches the file names the mechanisms are
known to ship (`conflict::KNOWN_FILES`). Each hit is reported with **its
mechanism and its owner** — the mod manifest's `mod_name` when the manifest
records the file, otherwise `Unmanaged` (the game itself, or a tool the launcher
did not install). Two mechanisms present is `has_conflict()`. A manifest that
cannot be read is a warning, never a failed scan.

`pitagames-cli mods conflict <dir> [--json]` is the diagnostic. The graphics
screen runs the same scan and lists each file and its owner in a pre-flight
card.

Editing the slot owner is recorded on the profile (`injection`), persisted, and
reversible through the editor's undo and the durable mod-manifest restore.
`app::graphics::slot_switch(previous, next, installed_mod)` reports the switch
and, when the previous owner installed files, returns an offer to uninstall them
(`SlotSwitch::offer`) — a Tier A previous owner is env-only and has nothing to
remove.

## The hard anti-cheat gate

The gate is enforced by the compiler, not only painted by the screen:

- `pitagames_core::profile::Profile::injection_confirmed` is the **recorded
  confirmation** (serde default `false`, so an older profile is unconfirmed).
- `platform::capability::InjectionGate` folds the effective anti-cheat and the
  curated EAC Linux v2 claim (`platform::anticheat::protected_kind`, which reads
  an `eac_v2` game as EAC even when the runtime field is unset) with that
  confirmation. `availability()` returns every injection field hidden
  (`ANTICHEAT_GATE`) while a protected game is unconfirmed, and `Unknown` never
  gates.
- `platform::envc::compile` returns `EnvcError::AntiCheatGate { anticheat }` when
  a profile that asks for injection is compiled for a protected game without the
  recorded confirmation — so **no injection env is emitted**.

The app writes the *effective* anti-cheat (detection merged with curation) onto
the compat entry it hands the matrix and the compiler, so a game curated `none`
but detected `eac` is gated too. The graphics screen shows the warning whenever
the game is protected and unconfirmed — even before an injection row is selected,
because a protected game hides those rows until the confirmation is recorded.

## Seams

| Crate | Responsibility |
|---|---|
| `core` (`pitagames-core`) | `SlotOwner`, `Profile::slot_owners`/`slot_owner`, `Profile::injection_confirmed` |
| `mods` (`pitagames-mods`) | `conflict::{scan, scan_dir, KNOWN_FILES, Mechanism, Owner, ConflictScan}` |
| `platform` (`pitagames-platform`) | `capability::{InjectionGate, reason::ANTICHEAT_GATE}`; `anticheat::protected_kind`; `envc::EnvcError::AntiCheatGate` |
| `app` (`pitagames`) | `graphics::{slot_switch, conflicts, anticheat_warning, injection_gate}`, `GraphicsEditor::set_injection_confirmed` |
| `cli` | `pitagames-cli mods conflict` |

## Verification

Pure crates (macOS host and CI):

```bash
cargo test -p pitagames-core -p pitagames-mods -p pitagames-platform -p pitagames-cli
cargo clippy -p pitagames-core -p pitagames-mods -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`crates/app` builds, is clippy-clean and passes its tests in `archlinux:latest`.

Not validated on hardware: the conflict names are the ones the three mechanisms
are documented to ship, and no real OptiScaler/HelixSR/DLSSNR install has been
scanned (ADR-0004).
