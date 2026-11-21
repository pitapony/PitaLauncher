# ADR-0010: One CLI driver for every store backend

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §4 (Epic/Amazon/GOG, `CliSpec`); `docs/m2-cli-backend.md`

## Context

Epic, Amazon and GOG are all driven by third-party CLIs (`legendary`, `nile`,
`lgogdownloader`/`gogdl`). Each has its own subcommands, its own progress
output, its own prompts and its own failure modes. Building three bespoke
runners would triple the hard code (PTY handling, timeouts, cancellation,
progress) and let the backends drift.

## Decision

Every store backend runs on one store-agnostic driver, `stores::cli`:

- **`CliSpec`** holds the program plus **one argv template per command**;
  `{id}`/`{dir}`/`{parent}`/`{name}` are substituted verbatim in **one pass**,
  never through a shell string. A command whose template cannot be filled (e.g. a
  `{dir}` the command cannot supply) is a typed `CliError::Template`, never a
  dropped argument; a command whose `CliSpec` template is *empty* is
  `CliError::Unsupported`, never an argument-less run.
- **`CliHandle::spawn`** runs the child on a **PTY** as a session leader (`setsid`
  with the PTY as its controlling terminal), a reader thread tees raw bytes to
  `cli-<program>-<command>-<nanos>-<pid>.log` and streams parsed
  `ProgressEvent`s, and `next_event`/`write_stdin`/`cancel`/`wait`/`drain` drive
  it.
- **Two locks** in the handle — the state and the reader's channel — because the
  poll waits on the channel outside the state lock. With one lock, a thread that
  only arrived between two polls was starved (a `cancel()` took ~12 s in the
  container) and an interactive flow could not answer a prompt while the main
  thread blocked.
- **Per-run timeouts:** idle 5 min + hard 6 h for installs, hard 10 min only for
  interactive auth. A timeout or `cancel()` sends `SIGHUP`, then `SIGKILL` to the
  process group after a 2 s grace.
- **Progress parsers** are one pattern per store (`Plain`/`Percent`/`JsonLines`);
  they are never inferred. `StoreStatus` (`Ready`/`CliMissing`) is probed from
  `PATH` here so every backend shares it.
- The child starts in `$HOME` unless `CliOptions::cwd` is set, so a relative
  `--program` does not resolve — use absolute paths.

Tests use `#!/bin/sh` fake CLIs injected through `CliOptions::env["PATH"]` and
synchronise through marker files, not sleeps.

## Consequences

- Positive: PTY handling, progress, timeouts, cancellation and transcripts exist
  once, and a new store is mostly a `CliSpec` plus parsers.
- Positive: never a shell, so no quoting bugs — the argv is a vector.
- Negative: the two-lock design is subtle and must be preserved; a
  straightforward-looking simplification reintroduces the starvation bug.
- The transcript can contain a pasted auth code or URL (the PTY echoes input);
  that is local diagnostics only and never enters SQLite or the UI.

## Alternatives considered

- **Shell strings per command.** Rejected — quoting/injection bugs; see
  ADR-0017.
- **A bespoke runner per backend.** Rejected — triples the hard code.
- **One lock on the handle.** Rejected after the container showed a ~12 s
  starvation on `cancel()`; the reader channel needs its own lock.

## References

- `docs/m2-cli-backend.md`; `crates/stores/src/cli.rs`.
- plan §4 *Epic / Amazon / GOG* (`CliSpec`).
- `docs/adr/0011`, `docs/adr/0016`.
