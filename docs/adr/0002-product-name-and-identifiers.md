# ADR-0002: Product name and identifiers

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §1 (Decisions already made); `AGENTS.md`; `packaging/PKGBUILD`

## Context

The project started as `game-launcher` and was renamed mid-flight. The GitHub
repository was already pushed as `PitaLauncher`, and the product needed one set
of names for binaries, XDG directories, the package and the AppStream ID.

## Decision

The product is **PitaGames**, and every identifier derives from it:

- Binaries: `pitagames` (GUI), `pitagames-cli` (headless diagnostics), and
  `pitagames-shim` (the launch shim, installed as
  `/usr/lib/pitagames/pitagames-shim` with a `/usr/bin/pitagames-shim` symlink so
  Steam launch options can name it).
- Crates: `pitagames-*` (`pitagames-core`, `pitagames-db`, …).
- XDG directory: `pitagames` — `<config>/pitagames`, `<data>/pitagames`,
  `<cache>/pitagames`.
- Package: `pitagames`; the AUR package is `pitagames-git` until there is a
  release tag (`provides`/`conflicts=('pitagames')` already match the release
  name).
- AppStream ID: `io.github.pitapony.PitaGames`, with
  `<launchable>pitagames.desktop</launchable>` so the desktop file keeps its
  plain name.
- The GitHub repository name stays **PitaLauncher**; the working directory and
  everything user-visible is `pitagames`.

Never introduce `gamel` or `game-launcher-*` identifiers.

## Consequences

- Positive: one name to grep for; no ambiguity in launch options, paths or
  package metadata.
- The working directory rename (`game-launcher` → `pitagames`, 2026-11-07) is
  complete; older references in history are historical only.

## Alternatives considered

- **Keep `game-launcher`.** Rejected — the product was named and the rename was
  cheap before release.
- **Rename the GitHub repo to match.** Unnecessary churn; the remote name is not
  user-visible and the ADR keeps the mismatch explicit.

## References

- `AGENTS.md` — the identifier rule and the packaging layout.
- `packaging/PKGBUILD` — package name, binary paths and the shim symlink.
- plan §1 *Decisions already made*, §10 *Packaging*.
