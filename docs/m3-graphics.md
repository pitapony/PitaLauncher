# M3 — the Graphics screen (`View::Graphics`)

Status: implemented and unit-tested (pure model) plus an Xvfb run at 1080p in
both shells. **Not driven against a real GPU**: the capability matrix is
exercised with fixture `GpuProfile`s, and the GUI's own GPU detection is the same
`platform::gpu` probe the CLI already uses. Per ADR-0004 nothing here is a
hardware claim.

## Why this slice exists

`platform::capability` (#17) answers one question per graphics field — *may we
offer this, and if not, why?* — and `platform::envc` (#18) compiles a whole M3
profile into a launch, refusing any field the matrix hides. This slice is the
**screen that lets a user make those choices** and see the honest answer for
their machine: the M3 user stories 13–24.

Three rules shape it:

- **Only what is available, with a reason the rest is not.** A hidden option is
  an absent row, not a greyed button (ADR-0004); the matrix's own reason string
  is listed beside the section. The *current* selection is always drawn, so a
  profile can never be a state the screen cannot represent.
- **One injection slot.** The screen does not re-implement ADR-0023: the matrix
  already hides DLSSNR while an injected upscaler owns the slot and vice versa,
  through the same `capability::availability` call the compiler consumes.
- **One composition.** The resolved command the screen previews is produced by
  the same `compile` + `compose_handoff`/`resolved_command` a real launch uses,
  so a preview and a Play cannot drift.

## Pieces

| Item | Role |
|---|---|
| `app::graphics` (`crates/app/src/graphics.rs`) | the framework-free model: `Section`, `Choice`, `rows`/`hidden`, `GraphicsEditor` (apply/revert/undo/reset/reload), `Control`, `injection_gate`, `durable_revert` |
| `app::graphics_view` (`crates/app/src/graphics_view.rs`) | the one Dioxus component, both shells |
| `app::runtime` (`runtime::graphics_preview`) | the graphics-aware compile and the one renderer |
| `app::navigation` (`Navigation::open_graphics`) | the route and its Back target |
| `app::detail` | the route in: the pad's `Context` (West) and the hero's **Graphics** button |
| `input` (`GRAPHICS_HINTS`, `hint::GAME_PAGE_HINTS`) | the cheat-sheet rows |

## The model

`graphics::rows(profile, gpu, compat, proton)` walks the closed `choices()` list
and keeps a choice when `capability::availability` says `Available`, **or** when
it is the profile's current value (a hidden current selection is drawn with its
reason so the state is representable). `graphics::hidden(...)` returns the ones
the matrix hides and the profile has not selected — the "Not offered here" box,
each with the matrix's reason verbatim. The `Choice` knows its `Field`, so the
section, label, "is this already selected", and "write it in" all live in one
place; a diagnostics toggle flips its bit, while an upscaler/frame-gen/injection
choice replaces its field.

`GraphicsEditor` holds the applied profile, the draft and the undo stack. Every
edit that changes the draft pushes the previous draft first, so undo returns to
a state the user actually saw; a press that changes nothing is not recorded.
`apply` validates the draft (the durable-on-disk value) before committing, so a
refused apply leaves the draft — and the applied profile — untouched.
`reset_to_safe_defaults` rebuilds the graphics fields at their defaults while
keeping the profile's identity, its Proton choice, its wrapper chain and the
advanced env overrides (which are not on this screen). `revert` throws the
unsaved draft away; the **durable** revert of files a Tier B injection wrote is
the mod manifest (`durable_revert` + `mods::manifest::restore`), surfaced as the
"Restore installed files" action only when a manifest exists.

## The one composition

`runtime::graphics_preview(game, app_id, draft, report, graphics)` runs the full
M3 compile (`envc::compile` with a real `GraphicsContext`) and then renders it:

- **Steam** → `resolved_command(app_id, &compiled)`, the same line
  `pitagames-shim --explain` prints and Steam's launch options carry;
- **non-Steam** → the compiled runtime becomes a `ShimProfile` and goes through
  the same `compose_handoff` the Play worker calls, so the umu/native/store-client
  line the screen shows is the line Play starts.

`runtime::compile` (the M1 runtime subset) delegates to the same
`compile_with_graphics` with an empty `GraphicsContext`, so a graphics field is
refused by name with the matrix's reason there, and the Graphics screen's
"Apply & verify" writes the shim file through `write_shim_profile_with_graphics`
— the file the shim and the hand-off read.

## The injection gate

`graphics::injection_gate(profile, anticheat, acknowledged)` is
`platform::anticheat::gate`. A draft that requests injection on a game the
effective anti-cheat (on-disk marker merged with the compat DB's curation) marks
protected cannot be applied until the user ticks the acknowledgement; `Unknown`
never hard-gates. The gate is consulted on both the click and the controller
seam, so a pad cannot apply what a click could not.

## Reaching it

The screen is opened *from* a game page: the desktop detail page's hero carries a
**Graphics** button and the pad's `Context` (West) opens it on either shell. It
does not touch the detail page's Back target, so `Back` from Graphics returns to
the page (whose own Back returns to the screen the game was opened from). The
screen is `View::Graphics`, not a `DESTINATIONS` entry — `Tab` cycles the
destinations and leaves it as `Detail` already does.

## Tests

- `crates/app/src/graphics.rs`: 18 pure-model tests — the availability rows and
  the hidden reasons, the current-selection rule, the single slot through the
  matrix, the editor's apply/revert/undo/reset/reload behaviour, the focus
  control list, the durable revert and the injection gate.
- `crates/app/src/runtime.rs`: the graphics preview compiles with a real GPU
  context, refuses a hidden field with the matrix reason, renders the non-Steam
  hand-off, and the graphics write accepts a profile the runtime subset refuses.
- `crates/app/src/navigation.rs`: `open_graphics` keeps the detail page's Back.
- `crates/input/src/hint.rs`: the Graphics cheat-sheet row and the game page's
  new **Graphics** entry.

Xvfb run at 1080p: `target-arch/graphics-smoke.sh` opens a seeded library, opens
the game page, opens Graphics, moves the ring, presses a row and reads the
resolved command.

## Known limits

- **No real GPU.** Every availability verdict is fixture-driven; the container has
  no AMD card, so the smoke run sees the "card not recognised" matrix.
- **"Apply & verify" verifies the compile, not engagement.** It validates the
  draft, persists it and writes the shim file; the "launch and report
  engaged/not" half is the M3.5 verification slice.
- **Tier B install is not wired here.** The screen offers `Injection::Manual` and
  reverts a manifest that exists; the deploy flow itself is the Tier B slice.
- **The pre-flight conflict scan is not on this screen** (later ticket).
- Not tested on a TV (ADR-0004).
