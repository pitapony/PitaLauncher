# ADR-0026: Open questions

- **Status:** Proposed
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §13 (Open questions & minor decisions)

## Context

Three items were left open when the rest of the design was finalized. They are
recorded here so a later decision supersedes this record instead of being lost in
the plan's §13.

## Decision

None of these is decided yet; the current defaults are:

1. **Telemetry / compat reporting.** Whether to offer an opt-in "share my working
   settings" export (a single game's env + GPU + Proton version, no account
   data). **Default: no telemetry at all in v1.** Revisit with a clear consent
   design.
2. **Console default on first run.** Assume **desktop**; console mode is opt-in
   (`--console`, start mode, `Guide`). Revisit if feedback says couch users are
   confused by a windowed first launch.
3. **Reverse-DNS AppStream ID.** **Resolved** for the package:
   `io.github.pitapony.PitaGames`, with `<launchable>pitagames.desktop</launchable>`
   so the desktop file keeps its plain name. Revisit only if hosting moves off
   GitHub.

## Consequences

- A later decision (e.g. shipping a telemetry export) should supersede this
  record with a new ADR rather than editing §13 in place.
- The resolved AppStream ID is already reflected in `packaging/` and ADR-0002.

## Alternatives considered

- **Decide now.** Rejected — all three need user feedback or a hosting decision,
  not more design.

## References

- plan §13.
- `docs/adr/0002` (AppStream ID), `docs/adr/0004` (compatibility claims).
- `packaging/` — metainfo.
