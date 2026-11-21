# ADR-0003: v1 scope and non-goals

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §1 (v1 scope, Non-goals for v1), §11 (Milestones)

## Context

A launcher can grow in every direction at once. The project needed a scope that
was large enough to be useful on the two target machines and small enough to
finish, and an explicit list of things it will *not* do so feature requests and
detours can be answered from the record rather than re-litigated.

## Decision

**v1 does:** the library (Steam scan + wrapped Epic/Amazon/GOG + imported local
titles), launching (the shim, the umu/Proton hand-off, per-game runtime
profiles), the graphics layer (upscaler/frame-gen/neural-rendering configuration
over Proton env), and the console shell (couch UI, controller input, in-app
on-screen keyboard, launch hand-off). Both a desktop windowed shell and a
console fullscreen shell ship in v1.

**v1 does not:**

- Reimplement Steam downloads (SteamCMD/DRM grey area), or run native
  Epic/Amazon/GOG protocol clients — those are wrapped behind a `StoreBackend`
  trait and driven by existing CLIs.
- Provide Steam Cloud sync, Discord RPC, achievements, or in-app store
  browsing/purchasing.
- Target Windows, macOS or Flatpak (Flatpak sandboxing breaks library scanning;
  revisit with portals).
- Support a bootable gamescope console session — that is v1.1; the in-app console
  shell ships in v1.
- Play video/trailer previews in the UI (would bind the UI to WebKit's GStreamer
  stack and block a native-renderer migration).
- Cover Steam Deck ergonomics, gyro/touchpad remapping, or multi-player "party"
  navigation.

## Consequences

- Positive: the milestone order (M0 → M1 → M2 → M2.5 → M3 → M3.5 → M4) has a
  bounded target; "later" has a clear meaning.
- Native store clients stay possible later, once a wrapped CLI is the blocker.
- The no-`<video>` rule is a hard constraint on the UI, not a preference (see
  ADR-0007 and ADR-0022).

## Alternatives considered

- **Native store protocol clients in v1.** Rejected — large and legally grey;
  wrapping `legendary`/`nile`/`gogdl`/`lgogdownloader` ships far sooner.
- **Bootable console session in v1.** Deferred to v1.1 — the in-app console shell
  delivers the couch experience without owning the compositor.
- **Flatpak.** Rejected for now — sandboxing breaks the Steam library scan.

## References

- plan §1 *Decisions already made* and *Non-goals for v1*, §11 *Milestones*.
- `AGENTS.md` — the store-wrapper and memory-footprint constraints.
- `docs/adr/0007`, `docs/adr/0022` — the renderer and console-shell records.
