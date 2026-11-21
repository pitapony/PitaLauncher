# ADR-0014: A failed listing never deletes rows

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** `docs/m2-store-sync.md`, `docs/m2-store-status.md`

## Context

A library sync reconciles what a store reports against the cached rows. The
naive rule — "the store's rows are exactly what this listing says" — turns any
transient failure into mass deletion: a network blip on an owned listing, or a
CLI that cannot list installs (GOG has none), would erase a user's library.

## Decision

A failed listing never deletes rows:

- A **failed owned listing writes nothing** for that store.
- A **failed install listing keeps a cached install's path and size** — a
  listing failure is not an uninstall.
- **A cached installed row the listing never mentions survives** (covers GOG,
  which has no install listing, and imported rows).
- `StoreLibrary { installed, owned, installed_known, owned_known, warnings }`
  folds the two listings; `rows()` is the fold and `merge(cached, fresh)` is the
  cache-aware reconciliation. An installed row wins over its owned twin (which
  only fills playtime).
- Writes go through one transaction:
  `GameRepository::replace_store_games(store, &[Game]) -> LibraryChange` upserts
  every row (preserving playtime via the shared `bind_upsert`) and then deletes
  **only that store's** rows the answer no longer names. Another store's rows are
  never touched, and a foreign-store id is refused before any write.
- `pitagames-cli stores sync` prints one line per store; a failed listing prints
  `skipped` with its warnings and still exits `0`.

## Consequences

- Positive: a sync can never nuke a library because a CLI hiccuped; deletion
  requires a *successful* listing that omits the row.
- Negative: a genuinely removed game can linger until a successful listing names
  its absence — the safe direction to be wrong in.
- An answered install listing still overrules a just-recorded install (a known
  ordering limit).

## Alternatives considered

- **Full replace per store.** Rejected — a failed listing would read as a mass
  uninstall.
- **Delete on any listing that omits the row, success or not.** Rejected — same
  failure mode, just narrower.

## References

- `docs/m2-store-sync.md`; `crates/stores/src/sync.rs`,
  `crates/db` (`replace_store_games`, `bind_upsert`).
- `docs/m2-store-status.md` — the unified update list that shares `UPDATE_TAG`.
