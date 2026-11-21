# ADR-0019: Process supervision and playtime accounting

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §5 (Process supervision); `docs/m2-launch.md`, `docs/m2-launch-history.md`

## Context

When PitaGames launches a non-Steam game it owns the process for the whole
session: it must capture output, know how the session ended, stop a running game
cleanly, and count playtime. A store game has no other playtime counter, so the
launcher's own measurement is the only one a non-Steam row ever gets.

## Decision

- **Spawn as a session leader** (`setsid`) so the whole process group can be
  signalled, and reap the child.
- **Tee stdout + stderr to a transcript** at
  `<logs>/<store>-<id>-<nanos>.log`. `Session::wait` returns
  `Outcome { code, signal, duration, log }` with a human `summary()`
  (`exited 0 after 1h 02m` / `stopped by signal 15 after 2s`).
- **Cancellation signals the whole group**: `SIGTERM`, then `SIGKILL` after
  `DEFAULT_GRACE` (10 s), from a detached thread.
- **Playtime = the child's wall-clock lifetime.** On `Finished`, the GUI adds
  `Outcome::duration` to the row's playtime via `persist::record_playtime` — the
  only playtime counter a store game has.
- The GUI half is `app::play` (framework-free `Play`/`Running`/`Last`,
  `PlayChannel`, one `pitagames-play-<store>` thread per game that owns the
  `Session`); `main.rs` drains it into the header strip and, on `Finished`,
  records playtime and (for the console shell) returns from the hand-off.
- **Every supervised session is recorded** in `launch_history`: one row written
  by the play drain — including a launch that never started, with its reason —
  carrying the title, start/end stamps, mode label, exit code, signal, duration,
  verdict and a **path** to the transcript (never its contents).

## Consequences

- Positive: a running game is always stoppable, its output is retained for
  diagnosis, and its playtime is measured without guessing from process trees.
- Positive: the History screen and a game's Recent launches card read from real
  sessions, failures included.
- Negative: the transcript can contain environment values, so only its path may
  be persisted (it can also carry a pasted auth code — ADR-0016).
- Steam sessions are absent from history: Steam starts its own games.

## Alternatives considered

- **Infer playtime from process trees / Steam only.** Rejected — Steam files do
  not cover store games.
- **Copy the transcript into SQLite.** Rejected — a game log can carry secrets.
- **`SIGKILL` immediately on stop.** Rejected — no chance for a clean exit;
  `SIGTERM` first, then a 10 s grace.

## References

- plan §5 *Process supervision*.
- `docs/m2-launch.md`, `docs/m2-launch-history.md`; `crates/platform/src/launch.rs`,
  `crates/app/src/play.rs`, `crates/db` (`launch_history`).
- `docs/adr/0016`, `docs/adr/0020`.
