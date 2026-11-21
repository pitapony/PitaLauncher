# ADR-0005: MIT license and the Rust 1.89 toolchain

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §1 (License); `AGENTS.md` (MSRV)

## Context

The project needed a license before it went public and a pinned Rust toolchain so
builds are reproducible across the macOS host, the `archlinux:latest` container
and the two target machines.

## Decision

- **License: MIT.** The initial commit kept its MIT `LICENSE`; `Cargo.toml`
  matches it.
- **MSRV is 1.89**, edition 2024 with let-chains. Two dependencies set the floor:
  `keyring` 4 needs 1.88, and the single-instance lock
  (`std::fs::File::try_lock`) needs 1.89.
- Keep `rust-version` in `Cargo.toml` at 1.89 or higher.
- Clippy's `collapsible_if` only suggests let-chains at this MSRV, which is why
  the pure crates use them; keep the style consistent rather than lowering the
  MSRV.

## Consequences

- Positive: reproducible builds pinned to one toolchain; let-chains are available
  throughout.
- The `try_lock`-based single-instance lock is only viable at 1.89+ — a lower
  MSRV would force a different locking primitive.

## Alternatives considered

- **Edition 2021 / MSRV 1.80.** Rejected — loses let-chains and the `try_lock`
  file-lock API the lock is built on.
- **No pinned MSRV.** Rejected — the container and the hosts must agree.

## References

- `Cargo.toml` — `license`, `rust-version`, `edition`.
- `AGENTS.md` — *Build & verify*, *MSRV*.
- `docs/m1-single-instance.md` — the lock that sets the 1.89 floor.
