# ADR-0028: Design tokens and the legibility floor

- **Status:** Accepted
- **Date:** 2026-11-22
- **Deciders:** julienpohie
- **Source:** plan §7 (Console mode), §8 (testing: "assert no text below the
  10-foot size floor"); the M2.5 "10-foot design pass" slice; ADR-0022

## Context

The two shells (ADR-0022) share one component set, but until this slice the
console shell only scaled its *base* font and header: every card, row, badge and
button carried a hard-coded `10px`–`16px` size, and a muted grey (`#78788a`) sat
on both backgrounds at about 3.5:1. A 10-foot shell is a claim about
**legibility** — type floor, contrast and hit areas — and a claim scattered as
literals across twenty files cannot be tested, so it cannot be kept.

Two constraints shaped the answer:

- The renderer boundary forbids injecting CSS from JavaScript.
- In this Dioxus build a `<style>` element written in RSX is **not applied**
  (verified with an Xvfb probe: a `div{background:#ff00ff}` rule never painted).
  So a global stylesheet is not available; only inline styles and the values in
  them reach the window.

## Decision

- **Tokens live in `pitagames_core::design`**, framework-free and unit-tested:
  `DesignTokens::for_shell(shell, &DisplayConfig)` resolves five type roles
  (`title`, `heading`, `body`, `caption`, `micro`), a hit-area minimum and the
  focus-ring width. Each role is `base × ui_scale`, then **clamped to the shell's
  floor**, so scaling the console shell down can never drop a caption below
  18 px (`CONSOLE_TYPE_FLOOR_PX`) or the desktop shell below 11 px.
- **The tokens are emitted as CSS custom properties** on the app root
  (`--text-*`, `--hit-min`, `--ring`, `--bg`, `--card`, `--text`, `--accent`, …)
  and components read the role, not a pixel size. Custom properties *do* work in
  this build; the probe that cleared them is recorded in `docs/m2.5-design.md`.
- **One palette with a contrast gate:** `BACKGROUND`, `CONSOLE_BACKGROUND`,
  `CARD`, `TEXT`, `TEXT_SECONDARY`, `TEXT_MUTED`, `TEXT_DISABLED`, `ACCENT`,
  `WARNING`, `DANGER`, `SUCCESS`, plus `Rgb::contrast_ratio` (WCAG 2.1 relative
  luminance). A test asserts every shipped text/background pair meets **AA
  (4.5:1)**, and the contrast math is checked against independent worked examples
  (black/white = 21:1, `#767676` on white ≈ 4.54:1). The old `#78788a` muted grey
  was replaced by `#9a9aac` because it failed.
- **The hit floor travels inline** (`shell::HIT_FLOOR` =
  `min-height:var(--hit-min);min-width:var(--hit-min);`) on every `cursor:pointer`
  control, because the global-rule form is unavailable. The desktop shell keeps
  28 px and the console shell 48 px; a disabled control is deliberately excluded.
- **Two invariants replace the stylesheet:**
  - `no_screen_ships_a_hover_only_affordance` — no `:hover`, `onmouseenter` or
    `onmouseover` in `crates/app/src`; a 10-foot shell has no pointer, so every
    action must be reachable from a pad.
  - `no_screen_ships_motion` — no `animation`, `transition`, `backdrop-filter` or
    `scroll-behavior` with a live value. Safe mode's "animations and blur off"
    (ADR-0007) therefore holds **by construction**; there is nothing to switch
    off, and the guard is what keeps it so.
- **`ui_scale` is a multiplier, not a licence to shrink:** the console floor wins.

## Consequences

- Positive: the console legibility claim is a tested number, not a scatter of
  literals; a palette change is checked against every surface at once.
- Positive: safe mode's motion guarantee became enforceable (a test) instead of
  aspirational (a stylesheet that never applied).
- Positive: one role name per size means the 10-foot pass is a mapping exercise
  over a file, not a redesign.
- Negative: the hit floor is repeated inline instead of declared once, so a new
  pointer control must remember `HIT_FLOOR`; the console-critical screens carry
  it today, and History/Account/Logs/Controls are a follow-up sweep.
- Negative: `min-width:var(--hit-min)` on a flex control changes its intrinsic
  width, which is intended here but rules out stylesheets-only portability.
- No TV is attached, so the floor is validated against 1080p/4K monitor proxies
  and remains "not tested on a TV" (ADR-0004).

## Alternatives considered

- **A global `<style>` element in RSX.** Rejected — verified not to apply.
- **Inject a stylesheet with JavaScript.** Rejected — the renderer boundary.
- **Leave the literals and check them with a lint.** Rejected: a lint cannot tell
  a 10 px caption from a 10 px icon glyph, and the role mapping is the point.
- **Scale everything by one multiplier.** Rejected: it makes a caption out of a
  body and cannot hold a floor.

## References

- plan §7 *Console mode*, §8 "assert no text below the 10-foot size floor".
- ADR-0007 (renderer boundary and safe mode), ADR-0022 (the console shell),
  ADR-0004 (compatibility claims, "not tested on a TV").
- `docs/m2.5-design.md`, `docs/m2.5-shells.md`, `docs/m2.5-safe-mode.md`.
