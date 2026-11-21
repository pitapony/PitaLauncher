# ADR-0021: Controller input is `gilrs`, not the webview Gamepad API

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §2 (dependency note), §7 (Navigation & input); `AGENTS.md` (Controller input)

## Context

Arch's `webkit2gtk-4.1` is built with `ENABLE_GAMEPAD` and depends on
`libmanette`, so `navigator.getGamepads()` *does* work inside the webview. That
would keep input in JS, next to the UI code. But it only reports while the webview
has focus, exposes no remap or hotplug metadata, and cannot be exercised
headlessly.

## Decision

- **Controller input goes through `gilrs` (evdev) in Rust**, in `crates/input`.
  **Do not use `navigator.getGamepads()`.** `gilrs` is the single input path for
  both the desktop and console shells.
- `crates/input` (`pitagames-input`) holds: the closed `Action` set (the one list
  the remap UI and the config iterate; only movement and tab switching repeat),
  the `Button`/`Key`/`Axis`/`Stick` bindings and per-device `InputMap`s (a *list*
  of bindings per action, a 0.35 radial deadzone, 400 ms/80 ms repeat), the two
  built-in layouts (`InputMap::gamepad`/`keyboard`), and TOML persistence of
  overrides at `<config>/pitagames/input.toml`.
- The hardware boundary is the `InputSource` trait: `GilrsSource` (the only place
  that touches a device) and a `ReplaySource` for tests. `Input::handle`/`tick`
  take an **injected `Instant`** so repeat rules are tested without sleeping.
  Devices are keyed by SDL GUID so a remap survives a replay; a disconnect
  releases everything the pad held.
- **Never hardcode button indices.** `gilrs`' SDL-derived DB plus per-device
  `InputMap`s handle non-standard pads; the remap UI (`RemapSession`, the one
  writer of `input.toml`) edits them.
- Navigation is a framework-free spatial `FocusGraph` (`crates/input/src/focus.rs`):
  cells at `(row, col)` ranked by same-row/column preference, per-axis wrap, a
  deterministic tie-break. Focus is a signal in `app`; **no DOM focus, no JS
  navigation** — the graph is unit-testable without a display.
- Keyboard maps to the same actions (`InputMap::action_for_key`), so both shells
  share one dispatch path.

## Consequences

- Positive: input is testable headlessly (`ReplaySource`), remappable, hotplug-
  aware, and works regardless of webview focus.
- Positive: one action set means the hint row (ADR-0022) and the remap UI cannot
  drift from what the pads actually emit.
- Negative: `gilrs` links `libudev`/`systemd-libs` (CI installs `libudev-dev`).
- Joystick access normally comes from logind `uaccess`; on failure show a
  one-line fix (add the user to `input`) rather than silently ignoring the pad.
  Never `EVIOCGRAB` — games must still receive input.

## Alternatives considered

- **`navigator.getGamepads()` in the webview.** Rejected — focus-only, no
  remap/hotplug metadata, untestable headlessly.
- **Hardcoded button indices.** Rejected — mapping varies across pads.

## References

- `AGENTS.md` — *Controller input*.
- plan §2 (the `getGamepads` note), §7 *Navigation & input*.
- `docs/m2.5-input.md`, `docs/m2.5-focus.md`, `docs/m2.5-gamepad.md`,
  `docs/m2.5-remap.md`.
