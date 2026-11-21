# ADR-0020: Three launch hand-off shapes — Umu / native / store client

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §5 (Runtime resolution); `docs/m2-launch.md`, `docs/m2-runtime-editor.md`

## Context

A non-Steam game is launched in one of three materially different ways, and the
GUI must preview exactly what Play will run. A Windows build needs Proton; a
native Linux build runs directly; and two backends (GOG, Amazon) name no
executable at all — their own client starts the game.

## Decision

`platform::launch::prepare(&Handoff)` composes an argv-only launch from the
store's `Game` row plus the *same* `profiles/<store>-<id>.json` the shim reads
(`platform::envc` stays the only compiler). There are three shapes:

- **`LaunchMode::Umu`** — `umu-run <exe>` with `GAMEID`/`PROTONPATH`/`WINEPREFIX`
  from `prefix::umu_env` and `STORE` in umu's own vocabulary
  (`epic`/`gog`/`amazon`/`steam`/`none`). A row is Windows when its exe ends in
  `.exe` or it carries the `platform:windows` tag.
- **Native** — a native executable runs directly with the profile's wrappers and
  no umu variables.
- **`LaunchMode::StoreClient`** — a row whose store names no executable: the
  hand-off carries a `LaunchSpec` composed by `stores::launch::client_spec` from
  the backend's own `CliSpec` (`gogdl launch <dir> <id> --platform windows`,
  `nile launch <id>`). `prepared.prefix` stays `None` because the store client
  owns its own Wine/Bottles runtime — the profile's environment and wrapper chain
  still apply, but no umu key is set, so a transparency panel never claims a
  prefix the client would ignore.

Rules:

- `prepare` **refuses**: Steam (it launches itself), an uninstalled row, a row
  with no executable *and no client*, an executable that vanished (one `stat`,
  the error names the path), and a Windows build with no Proton.
- **umu values replace the profile's rather than duplicating them** (`push_env`
  overwrites in place), so a stale `GAMEID` cannot point two games at one prefix.
- `runtime::compose_handoff(game, runtime, report)` is the **one** composition
  shared by `app::play`'s worker and the detail page's Command panel, so the
  previewed line and the started process cannot drift.
- `stores::launch::launches_by_id(store)` is the **one** predicate the GUI's
  `play_is_wired()` and `runtime::client_launch` ask.
- Wrapper chains flatten in front of `umu-run` via `core::launch`;
  `PITAGAMES_SESSION=1` drops gamescope steps only (nested gamescope is a bug).

## Consequences

- Positive: one composition, one predicate, one preview — the detail page can
  never show a command Play would not run.
- Positive: a client launch is honest about owning its own runtime (no phantom
  prefix).
- Negative: `StoreClient` is a black box — the store picks its own runtime, so the
  per-game prefix applies only to the environment.
- A fresh store install is playable only after the next library sync (the
  executable comes from the listing).

## Alternatives considered

- **One shape (always umu).** Rejected — native games need no Proton, and GOG/
  Amazon rows name no executable.
- **A separate preview path for the UI.** Rejected — guarantees drift; hence one
  `compose_handoff`.

## References

- `docs/m2-launch.md`, `docs/m2-runtime-editor.md`; `crates/platform/src/launch.rs`,
  `crates/stores/src/launch.rs`, `crates/app/src/runtime.rs`.
- plan §5 *Runtime resolution*.
- `docs/adr/0015`, `docs/adr/0017`, `docs/adr/0019`.
