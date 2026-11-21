# ADR-0016: The in-app sign-in terminal is one shared relay

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** `docs/m2-auth-terminal.md`; `AGENTS.md` (in-app sign-in terminal)

## Context

Each store CLI has its own sign-in flow: a pasted code (`legendary`), a browser
sign-in plus pasted redirect URL (`nile`, `lgogdownloader`), an ENTER prompt, and
a refusal that still exits `0`. Both the headless CLI and the GUI need to drive
these flows, and two implementations would inevitably answer different prompts.

## Decision

`stores::auth` is the one relay both the CLI and the GUI drive:

- `has_sign_in(store)`, `spec_for(store)`, `hint(store)` (the sentence that says
  what to paste), `log_format(store)`.
- `AuthRun::{start,attach,next,answer,cancel,finish,log_path}` and
  `AuthOutcome::{succeeded,cancelled,failure_message}`.
- **`next()` is the one prompt policy:** a line containing `Press ENTER` is
  auto-answered and reported as `AuthStep::Continued`; a line ending in `:`
  becomes `AuthStep::Prompt` with the store's hint; everything else is a `Line`.
  The verdict is `RunReport::succeeded()` in the store's layout, so a refusal
  that exits `0` is not a success.
- `app::auth` is the framework-free terminal model
  (`AuthEvent::{Line,Prompt,Finished}` tagged with a run id, `AuthTerminal`,
  `AuthStatus`, `AuthInputs`, `AuthChannel`, `next_run()`, `spawn`): one reader
  thread drives `AuthRun::next`, one writer thread applies typed lines, both over
  one `Arc<AuthRun>`. A report for another run id is ignored.
- **CLI and GUI cannot drift:** the three CLI sign-in commands were rewritten onto
  the relay.
- **Closing the panel cancels the CLI** (the writer cancels when the window drops
  the inputs); one sign-in at a time.
- **Auth transcripts are local diagnostics only** — the PTY echoes the pasted
  code/URL, so it must never enter SQLite or the UI as data (redaction is M4).

## Consequences

- Positive: a prompt-policy change lands once and both surfaces answer
  identically; ~150 lines of duplicated prompt handling were deleted.
- Positive: a late report from a previous run cannot corrupt the current one.
- Negative: the relay is auth-only (no cursor control/history); a general PTY for
  mod installers is M3.
- The pasted secret is visible in the panel and the transcript — accepted for now.

## Alternatives considered

- **A separate GUI auth implementation.** Rejected — the prompt drift is exactly
  what breaks a fragile CLI flow.
- **Block on the CLI in the UI thread.** Rejected — the two-lock handle and the
  writer thread exist so the UI stays responsive and can cancel.

## References

- `docs/m2-auth-terminal.md`; `crates/stores/src/auth.rs`,
  `crates/app/src/auth.rs`; `docs/m2-cli-backend.md`.
- `docs/adr/0010`, `docs/adr/0011`.
