# ADR-0006: Twelve-crate workspace and the framework-free boundary

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §2 (Workspace layout, Dependency choices); `AGENTS.md` (Renderer boundary)

## Context

The GUI, the launch shim and the headless CLI share a domain model, but only the
GUI needs a heavyweight webview renderer. Most of the value (VDF parsing,
launch composition, store integration, input) must be testable and buildable on
a machine with no display — this host, and CI.

## Decision

- **One workspace, twelve crates:** `core`, `db`, `stores`, `platform`, `mods`,
  `metadata`, `art`, `secrets`, `input`, `app`, `shim`, `cli`.
- **Only `crates/app` may use Dioxus/RSX.** `core`, `db`, `stores`, `art`,
  `platform`, `mods`, `metadata`, `input`, `shim` and `cli` stay framework-free —
  zero Dioxus types, no JS navigation, no DOM focus, no `localStorage`. The same
  rule covers the CLI (`pitagames-cli`) and the shim (`pitagames-shim`), which
  build and run on a machine that has neither a webview nor a GUI toolchain.
- Split by testability, not by ceremony: `secrets` is separate so the `keyring`
  dependency and the file-fallback rules stay out of the GUI; `art` is separate
  from `metadata` so the image stack and the loopback server stay out of the
  appdetails client. Do not split `stores` per backend until a backend exceeds
  ~2k lines.
- Dependencies are fixed per the plan's table (`dioxus` 0.7, `gilrs`, `sqlx`,
  `reqwest` + `rustls`, `portable-pty`, `tokio`, `notify`, `axum`, `image`,
  `thiserror`/`anyhow`, …).
- **No Tailwind in v1** — plain CSS with custom properties and a small
  design-token file; fewer build steps and no Node requirement beyond `gogdl`.

## Consequences

- Positive: the pure crates build, test and lint on macOS and in the container
  without the webview toolchain; a future renderer swap is a port of markup and
  CSS rather than a rearchitecture.
- Positive: input comes from `gilrs`, focus from the graph in `input`,
  persistence from SQLite — none of it crosses the Dioxus boundary.
- Negative: a new feature must be placed deliberately (a pure model plus a thin
  `app` seam), which is more design work upfront.
- `crates/app` is not built locally on macOS (see `AGENTS.md` build list).

## Alternatives considered

- **A single crate.** Rejected — the CLI/shim would drag in the webview and the
  renderer boundary would erode.
- **One crate per store backend.** Deferred — unnecessary until a backend is
  large.
- **Tailwind/Node for styling.** Rejected — extra build step, and Node is only
  wanted as an optional dependency for `gogdl`.

## References

- plan §2 *Workspace layout* and *Dependency choices*.
- `AGENTS.md` — *Non-negotiable design constraints → Renderer boundary*, and the
  local build list.
