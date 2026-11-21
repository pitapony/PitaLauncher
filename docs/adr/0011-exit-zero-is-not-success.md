# ADR-0011: Exit code 0 is not success

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** `docs/m2-cli-backend.md`, `docs/m2-epic.md`, `docs/m2-amazon.md`, `docs/m2-gog.md`

## Context

The wrapped store CLIs are inconsistent about failure. `legendary` and `nile`
log an error and still exit `0` for many refusals; `gogdl` exits non-zero. A
launcher that trusts exit codes would report "installed" for a failed install and
"verified" for a corrupt one.

## Decision

Never trust the exit code alone; judge every run through the shared verdict layer
`stores::report`:

- **`LogFormat::Legendary`/`Nile`/`Gogdl`** parse each CLI's line layout
  (`[cli] INFO: …`, `INFO [CLI]:\t …`, `[NAME] INFO:`); `gogdl` reuses the
  bracketed `Legendary` shape.
- **`RunReport::succeeded()`** = exit `0` **and** no line at `ERROR`/`CRITICAL`/
  `FATAL`. This is the verdict for library, install, verify and sync.
- **`RunReport::exited_cleanly()`** = exit `0` only, used for `launch`, because
  the game shares the PTY and its own output must not be read as a failure.
- **`verify` verdicts are parsed from the summary line** (`VerifyReport`),
  because `legendary` exits `0` for corrupt/missing files. Amazon's `verify` is a
  repair whose verdict is its error lines.
- **`nile launch` needs `amazon::launched()`** — exit `0` *and* nile's own
  `INFO [LAUNCHER]: Launching` marker — because a refusal also exits `0`.
- `extract_json` tolerates a PTY-merged `[cli] INFO:` stream, since JSON and log
  noise share one tty.

Each backend documents its own traps (see the References).

## Consequences

- Positive: an exit-0 refusal can never be reported as success; the rule lives in
  one module and every backend opts into the same predicate.
- Negative: per-store verdict logic (summary lines, markers) means a new backend
  must be studied, not assumed.
- A run's raw transcript is retained for diagnosis anyway (ADR-0010).

## Alternatives considered

- **Trust the exit code.** Rejected — `legendary`/`nile` exit `0` on refusals.
- **Treat any `ERROR` line as failure for `launch` too.** Rejected — a running
  game legitimately logs errors; `launch` uses `exited_cleanly()` (or a store
  marker) instead.

## References

- `docs/m2-cli-backend.md` — `stores::report`.
- `docs/m2-epic.md` — the two `legendary` exit-0 traps.
- `docs/m2-amazon.md` — the `Launching` marker and verify-as-repair.
- `docs/m2-gog.md` — `gogdl` exits non-zero, so `exited_cleanly()` is enough.
