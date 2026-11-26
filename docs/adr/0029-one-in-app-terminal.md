# ADR-0029: One in-app terminal, generalized from the sign-in panel

- **Status:** Accepted
- **Date:** 2026-11-25
- **Deciders:** julienpohie
- **Source:** `docs/m2-auth-terminal.md`; spec `pitapony/PitaLauncher#8`, issue `#10`

## Context

The in-app sign-in terminal (`app::auth`) already had everything an interactive
child process needs — a streamed line, one typed line, Cancel, a run id so a late
report is ignored, and the reader+writer threads the driver's two-lock handle
exists for. But it was named for, and coupled to, `stores::auth::AuthRun`: the
M3/M3.5 mod installers (`install.sh` run in a PTY, with unknown prompts and
captured `%command%` lines) would have copied it, and copied prompt policy drifts.

## Decision

Generalize the terminal model into `app::terminal` and make the store sign-in one
caller of it:

- `terminal::{TerminalEvent, Terminal, TerminalStatus, TerminalInputs,
  TerminalChannel, next_run}` are framework-free and know nothing about what they
  drive. `TerminalEvent::{Line,Prompt,Finished}` is run-id tagged;
  `Terminal::apply` ignores another run's report and returns whether the run
  completed **successfully** (the caller's cue to act on a win).
- **`TerminalDriver`** is the seam: `next_step -> Result<Option<TerminalStep>, _>`,
  `answer`, `cancel`, `finish -> Result<TerminalOutcome, _>`. A store CLI, an
  installer, or a fixture implements it.
- **`terminal::spawn(run, name, start, events)`** runs `start()` **inside the
  worker thread** (so a slow or failing spawn is reported as a failed run, not a
  blocked click), then drives the reader and writer threads over one
  `Arc<dyn TerminalDriver>`. Dropping `TerminalInputs` cancels the run — the same
  cancel-on-close the sign-in panel relied on.
- `app::auth` is now the thin adapter: it maps `stores::auth::AuthStep` →
  `TerminalStep` and `AuthOutcome` → `TerminalOutcome`, re-exporting the relay's
  prompt policy unchanged. The Stores screen keeps its modal, but renders a
  `Terminal` and remembers the running store separately (`sign_in_store`) so a row
  can show `signing in…`.

## Consequences

- Positive: one terminal for sign-in and (from M3) the mod installers; the second
  caller is proved by a scripted test-double `TerminalDriver` and, in the GUI, by
  the same sign-in panel.
- Positive: the store sign-in behaviour is unchanged — same prompt policy, same
  verdict, same re-probe on a stored session, same cancel when the panel closes.
- Negative: one more indirection (`TerminalDriver`) between the terminal and the
  CLI; accepted for the reuse.

## Alternatives considered

- **Keep `app::auth` and copy it for the installers.** Rejected — the drift this
  record exists to prevent.
- **A general in-app PTY (ANSI, cursor control, history).** Still M3/M4; this is a
  stream + one line + Cancel, which is what both callers need.

## References

- `docs/m2-auth-terminal.md`; `docs/adr/0016-in-app-sign-in-terminal-relay.md`.
- `crates/app/src/terminal.rs`, `crates/app/src/auth.rs`,
  `crates/app/src/stores_view.rs`.
