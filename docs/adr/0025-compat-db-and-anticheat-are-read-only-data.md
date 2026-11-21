# ADR-0025: Compat DB and anti-cheat are read-only curated data

- **Status:** Accepted for the data model and the pipeline; **Proposed** for
  enforcing the injection gate (the graphics screen is M3)
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §6 (Compat DB), §12 (anti-cheat risk); `AGENTS.md` (compat DB, anti-cheat)

## Context

Per-game knowledge (engine, supported upscalers, preferred DLL, anti-cheat,
known issues) is curated, shipped with the package and must also be overridable
by a user. Anti-cheat status affects whether DLL injection is safe. Neither can
depend on the GUI or on a database the CLI lacks.

## Decision

- **The compat DB is files, not SQLite** (ADR-0008): shipped `compat/*.toml`
  (baseline) + `<config>/pitagames/compat/*.toml` (user overrides), merged
  **field-by-field** and validated with warnings. `PITAGAMES_COMPAT_DIR` replaces
  the shipped dirs so a checkout works without an install; `pitagames-cli compat`
  is the diagnostic, and a test asserts the repository's own `compat/` loads
  warning-free.
- **Strict typed validation at load:** unknown keys/values are rejected, not
  ignored, and out-of-range values are clamped with a warning. A malformed file
  is a warning plus defaults, never a crash.
- Entries are marked with their validation status (`experimental`,
  `unverified`) per ADR-0004.
- **Anti-cheat detection is read-only and heuristic:** `platform::anticheat`
  walks a bounds-limited game directory (depth ≤ 3, ≤ 50k entries) for EAC/
  BattlEye markers and **merges** them with the compat DB's curated value — a
  real marker wins, otherwise curation is trusted (anti-cheat can live outside the
  game folder). `pitagames-cli anticheat <appid>` is the diagnostic; the detail
  page shows the card and the warning.
- **`Unknown` never hard-gates.**
- **Curation hard-gates injection behind an explicit confirmation dialog naming
  the risk** (`eac`/`battleye` → `AntiCheat::is_protected` +
  `Profile::requests_injection`; never auto-enable; never inject in online games
  by default).
- **The gate is not enforced yet** (Proposed): `platform::anticheat` ships the
  detection + policy, but the graphics screen that would act on it is M3.

## Consequences

- Positive: the compat DB can ship in the package, be edited by hand and be read
  by the CLI without a database; the same data drives the UI's option
  availability.
- Positive: real anti-cheat markers override stale curation, and the honest
  `Unknown` state never blocks a legitimate launch.
- Negative: two sources to merge, and every field must be validated (more code
  than deserializing into a struct and trusting it).
- Until the gate is enforced, the card and warning are informational only.

## Alternatives considered

- **Store the compat DB in SQLite.** Rejected — it is shipped, overridden and
  hand-edited (ADR-0008).
- **Trust curated anti-cheat alone.** Rejected — markers on disk are more current.
- **Hard-gate on `Unknown`.** Rejected — many unknown games are fine; that would
  break legitimate use.

## References

- plan §6 *Compat DB*, §12 (anti-cheat guardrail).
- `AGENTS.md` — *The compat DB is files*, *Anti-cheat detection is read-only*.
- `docs/m1-compat-db.md`, `docs/m1-anticheat.md`.
- `docs/adr/0004`, `docs/adr/0008`, `docs/adr/0023`.
