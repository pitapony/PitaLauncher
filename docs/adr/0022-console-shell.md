# ADR-0022: The console shell

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §7 (Shell, Console mode); `AGENTS.md` (console-shell bullets)

## Context

PitaGames targets a couch (10-foot) experience as a first-class posture, not a
bonus. The same data and routes should serve a windowed desktop use and a
controller-only fullscreen use, and the launcher must step aside while a game
runs without losing its place when the game exits.

## Decision

- **Two shells over one component set:** `Shell::Desktop` (windowed, decorated,
  1280×800 default) and `Shell::Console` (undecorated fullscreen with a TV
  overscan/safe-area margin), switched **at runtime without a restart**. A
  persisted `StartMode::{Desktop,Console,Remember}`, `--console`/`--desktop`,
  and `Guide`/`F11` all reach it; the display numbers live in
  `<config>/pitagames/config.toml` (`DisplayConfig`).
- **Console home** is a full-bleed **Continue** hero plus horizontally-scrolled
  shelves (Recently played, Installed, one per store), using the *same*
  `start_play`/`start_install` helpers the grid and detail page use, and a detail
  page's Back returns to the screen it was opened from.
- **Text entry** is an in-app grid **on-screen keyboard** (bottom sheet, two-layer
  ragged key grid walked by the same `FocusGraph`), opened by `Action::Search`
  (keyboard `F`, pad `North`); dialogs are modal and focus-trapped with no
  hover-only affordances.
- **Launch hand-off:** launching hides the window and pauses the input pump; on
  game exit the window is shown and focus requested via an activation token. If
  the compositor refuses, a **"game exited — press Guide"** badge shows on next
  focus. Because an unmapped WebKitGTK window freezes the Dioxus virtualdom, the
  window is **mapped again from the game's worker thread** before its `Finished`
  report, not from a future.
- **The overlay** (pad `Start`, keyboard `M`) is the system menu: resume, the
  running/open game's detail page and (Steam-only) store page, the shell switch,
  and (disabled until v1.1) power rows, with a controller + battery line and a
  clock. While it is open the pumps route **every** action into it.
- **No `<video>`** in the v1 UI (would bind the UI to WebKit's GStreamer stack
  and block the native-renderer migration).
- The last-input glyph hint row and the 2 s auto-hidden cursor follow the device
  used last, so a remap changes the row with no second table.
- **Safe mode** (`--safe-mode`) and the 10-foot design pass are the remaining
  M2.5 slices (ADR-0007).

## Consequences

- Positive: one screen set serves both postures; the couch path is exercised by
  the same code as the desktop path.
- Positive: the hand-off avoids the frozen-virtualdom trap and degrades to a
  visible badge when the compositor refuses focus.
- Negative: the console UI costs ~300 MB of WebKit while a game runs — mitigated
  by hiding the window during play.
- Negative: no TV is attached, so 10-foot claims ship "not tested on a TV"
  (ADR-0004).
- Console power rows are placeholders until the v1.1 gamescope session owns them.

## Alternatives considered

- **Separate console app.** Rejected — duplicates every screen and drifts.
- **Require a reboot/restart to change shells.** Rejected — `Guide`/`F11` toggling
  is cheap and expected.
- **`<video>` previews in the console UI.** Rejected (see above).

## References

- plan §7 *Shell*, *Console mode*, *Navigation & input*.
- `AGENTS.md` — the console-shell and hand-off bullets; *No `<video>`*.
- `docs/m2.5-shells.md`, `docs/m2.5-home.md`, `docs/m2.5-osk.md`,
  `docs/m2.5-handoff.md`, `docs/m2.5-overlay.md`, `docs/m2.5-hints.md`.
- `docs/adr/0021`.
