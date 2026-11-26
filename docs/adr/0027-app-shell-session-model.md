# ADR-0027: The app shell session model and the screen seam

- **Status:** Accepted
- **Date:** 2026-11-21
- **Deciders:** julienpohie
- **Source:** architecture review 2026-11-21 (churn-scoped to `crates/app/src/main.rs`);
  `docs/m2.5-shell-model.md`

## Context

`crates/app/src/main.rs`'s `App()` grew into the churn epicentre of the repo:
1,741 lines in one component (`main.rs:273–2013`), 35 `use_signal` values and 12
`use_future` drains, with navigation, the per-screen action channels, the
hand-off, the overlay and the shared start helpers all inside it.

Every screen is a Dioxus component that receives an explicit props struct of
**signals `App` owns** — `HomeViewProps` 16 fields, `DetailProps` 13,
`ControlsViewProps` 8, `AccountProps` 7, `OverlayViewProps` 7, `StoresViewProps`
6, `OskSheetProps` 5, `HistoryViewProps` 3, `LogsViewProps` 2 — roughly 55
signal fields threaded down. `DetailProps` even documents *"navigation stays in
`main.rs`"*. There is no `use_context_provider` anywhere in the crate.

The consequences the review named: the root's interface is as complex as its
implementation (**shallow**); understanding one concept ("play a game", "open a
game") means bouncing between `main.rs`, `play.rs`, `runtime.rs`, `handoff.rs`
and `store_installs.rs`; `start_play`/`start_install`/`commit_search` are
`pub(crate)` in `main.rs`, so a screen depends **upward** on the root; and the
missing "how does a screen apply an `Action`" seam is restated once per screen
(`perform_action`, `handle_downloads_action`, plus a drain in each screen
module).

The repo already has the shape that fixes this, used four times over:
framework-free model modules inside `crates/app` (`play.rs`, `handoff.rs`,
`overlay.rs`, `store_installs.rs`, `store_sync.rs`, `home.rs`, `grid.rs`,
`downloads.rs`) paired with a thin Dioxus seam and unit-tested with no display.
ADR-0021 describes the same model-plus-seam split for `crates/input`.

## Decision

Deepen the app shell into a **framework-free session model** plus a thin Dioxus
seam, following the pattern the crate already uses:

- **`navigation.rs`** (framework-free) owns `View` (moved out of `main.rs`) and
  `Navigation`: the current view, the back target, the selected game, the
  library query and the text-focus flag. "Where am I and what did I open" is one
  module, not six signals.
- **`session.rs`** (framework-free for the navigation half) owns a `Session`:
  the `Navigation` plus the shared concern handles (`Play`, `Installs`,
  `Downloads`, the input maps and their reload handle, the history revision)
  and `notice`, behind **one `dispatch(Action)`**. This is the screen seam: a
  screen implements `handle(Action)` and the session routes to it. The
  per-screen `ActionChannel` split from ADR-0021 stays as it is — the deepening
  adds the missing `handle` seam *above* it, it does not re-litigate the split.
  *(Update, ticket #9: the handles are held as `Signal`s, so the `Session` is
  now part of the Dioxus seam — only `Navigation` stays framework-free. Its
  tests build a `Session` on a headless `VirtualDom`.)*
- **`context.rs`** is the Dioxus seam: `provide_session` / `use_session` over
  `use_context_provider` (confirmed present in Dioxus 0.7.10). A screen receives
  **one handle**, not a 2–16-field props struct of signals the root owns.
- **`pumps.rs`** owns the drain set; `App()` **registers** the pumps instead of
  implementing nine loops inline. A drain that touches only one concern (the
  play drain, the install drain) moves beside that concern.
- **`launch_start.rs`** owns `start_play`/`start_install`, so screens depend on
  a module instead of upward on the root.
- The **overlay, the hand-off and safe mode stay separate framework-free
  models** the session consults; they are deep and tested already (ADR-0007,
  ADR-0022), and folding them in would reopen settled decisions.
- The model lives in **`crates/app`, not `core`**: it holds app-level concerns
  (`Play`, `Installs`, `Downloads`), and `crates/app` is bin-only, so its tests
  run **headlessly in the Arch container** (`cargo test -p pitagames-app`), the
  same way every existing app model is tested — not on macOS.

The work lands **incrementally, screen by screen**, each step built and
smoke-run in the container: `navigation.rs` → `session.rs` + `dispatch` →
`context.rs` → `pumps.rs` → `launch_start.rs` → screens smallest-first →
delete dead code.

## Consequences

- Positive: the root gains a small interface (`view`, the session handle, one
  `dispatch`) over a large implementation — **depth** at the shell's interface.
- Positive: **locality** — navigation, dispatch and the start paths each live in
  one module; a screen can be read without reading the root.
- Positive: the model is testable without a display, so dispatch and navigation
  rules get tests the current root cannot have.
- Positive: the interface is the test surface — a new screen wires up by
  touching **one** file (its own adapter), which is the property the deepening
  exists to buy.
- Negative: `crates/app` stays container-only for tests; the model is not
  verifiable on the macOS host.
- Negative: `session::Session` shares a word with `platform::launch::Session`.
  The launch type is always written module-qualified (`launch::Session`), so the
  two do not collide; the name is revisited if that ever stops being true.
- Negative: one more layer between `App()` and a screen, so a deep link into a
  screen's internals costs one indirection.

## Alternatives considered

- **Dioxus-idiomatic split only** (keep signals, add context providers, make
  `App` a router). Rejected — smaller, but the shell stays untestable off the
  webview, which is the main thing the deepening is for.
- **A new `crates/shell` crate.** Rejected — contradicts ADR-0006's fixed
  twelve-crate workspace, and the model needs `crates/app`'s own types.
- **Put the model in `crates/core`** so it is macOS-testable. Rejected — it
  would drag app-level concerns (`Play`, `Installs`, `Downloads`) into the
  domain crate; `core::ui` is precedent only for display *configuration*, not
  for the launcher's screen state.
- **A big-bang rewrite of `App()`.** Rejected — 1,741 lines with no local build
  makes an unverified intermediate state likely; incremental steps keep every
  diff smokable.
- **Fold the overlay / hand-off / safe mode into the session interface.**
  Rejected — they are already deep, tested models with their own ADRs.

## References

- `docs/m2.5-shell-model.md` — the slice doc.
- `AGENTS.md` — the renderer boundary and the `crates/app` build/test notes.
- ADR-0006 (framework-free boundary), ADR-0007 (renderer + safe mode),
  ADR-0021 (controller input: model plus seam), ADR-0022 (console shell).
- `crates/app/src/main.rs`, `crates/app/src/{play,handoff,overlay,home,grid}.rs`.
