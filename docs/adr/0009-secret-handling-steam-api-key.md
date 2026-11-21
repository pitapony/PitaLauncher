# ADR-0009: Secret handling — the Steam Web API key

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §4 (Steam, owned games); `AGENTS.md` (Secrets never leak)

## Context

The only secret PitaGames holds is the optional Steam Web API key (used for
`IPlayerService/GetOwnedGames`). A leaked key in a log, a JSON dump, an error
message or a support bundle is a real incident, and a container has no desktop
keyring session.

## Decision

- The key is the `pitagames_secrets::SteamApiKey` type: **no `Display`**, a
  **redacting `Debug`**, and the raw value reachable only through
  `expose_secret`.
- Resolution order: **`PITAGAMES_STEAM_API_KEY` environment** → **desktop
  keyring** → **`0600` fallback file** at
  `<config>/pitagames/secrets/steam-api-key`.
- Every API error is scrubbed of the key before it is surfaced.
- Never log, print or JSON-serialize a key. The UI shows `masked()` only.
- The fallback file is written `0600`; the container's "no keyring → fallback
  file with a warning" path is expected, not a bug.

## Consequences

- Positive: the key cannot be leaked accidentally through the ordinary
  formatting/logging paths, because those paths cannot see it.
- Positive: headless/container use still works through the env var or the file.
- Negative: callers must call `expose_secret` explicitly, which is slightly
  clumsier than a plain `String` — intentional friction.
- The `secrets` crate is separate so `keyring` and these rules stay out of the
  GUI and the stores layer (ADR-0006).

## Alternatives considered

- **Store the key in SQLite or `config.toml`.** Rejected — plaintext at rest in a
  file that is meant to be user-visible/editable.
- **Keyring only.** Rejected — no session keyring in a container or over SSH.

## References

- `crates/secrets`, `docs/m1-owned-games.md`.
- plan §4 *Steam* (owned games), *Probable next steps*.
- `AGENTS.md` — *Secrets never leak by accident*.
