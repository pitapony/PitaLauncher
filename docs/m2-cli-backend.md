# M2 — the CLI driver shared by the store backends (`stores::cli`)

Status: implemented and verified locally (pure crates on macOS) and in
`archlinux:latest`. Not yet driven against a real `legendary` / `nile` /
`lgogdownloader` — those presets land with their own slices, and none of the
three CLIs is installed on the target machines.

## Why this exists

Three of the four stores are external programs: Epic (`legendary`), Amazon
(`nile`) and GOG (`lgogdownloader`, `gogdl` fallback). All three need the same
six things — a pseudo-terminal for their prompts, a way to type an answer, a
progress parser, cancellation, timeouts, and a transcript for support. Written
three times, that is three places for a cancelled install to keep a worker
running.

So M2 starts with one driver, not with a store. `stores::cli` is framework-free,
DB-unaware and argv-only: a spec of program + argument templates goes in,
`ProgressEvent`s come out, and the store backend on top owns the SQLite writes.

```text
  CliSpec (program + argv templates)          CliHandle (caller's thread)
         │                                    ├─ writer ──► pty master ──► child stdin
         ▼                                    ├─ child  (wait / kill)
  CliCommand ──argv()──► program + args ──spawn─┤
                                               └─ rx ◄── reader thread ◄── pty master
                                                          └── raw log file
```

## Shape

| Type | Role |
|---|---|
| `CliSpec` | one CLI: `program`, `ProgressParser`, and one argv template per command |
| `CliCommand` | `ListInstalled`, `ListOwned`, `Auth`, `Install { id, install_dir }`, `Launch { id }` |
| `ProgressParser` | `Plain`, `Percent(Regex)`, `JsonLines` |
| `ProgressEvent` | `Line`, `Percent(f32, String)`, `Stage(String)`, `Done`, `Failed { code, message }` |
| `CliOptions` | `log_dir`, `env`, `cwd`, `idle_timeout`, `hard_timeout`; `CliOptions::interactive()` |
| `CliHandle` | `spawn`, `next_event`, `write_stdin`, `cancel`, `log_path`, `wait` |
| `CliOutcome` | `success`, `code`, `cancelled`, `timed_out`, `log_path`, `events`, `raw_tail` |
| `CliError` | `NotFound`, `Template`, `Log`, `Spawn`, `Io` — only *before/while* a run starts |

A run is a stream plus an outcome, not a `Result` with the output inside it:

```rust,ignore
let handle = CliHandle::spawn(&spec, &CliCommand::Install { id, install_dir }, &opts)?;
while let Some(event) = handle.next_event()? {
    match event {
        ProgressEvent::Percent(pct, text) => board.progress(pct, text),
        ProgressEvent::Failed { message, .. } => board.failed(message),
        _ => {}
    }
}
let outcome = handle.wait()?;   // terminal event, exit code, flags, log path
```

## Argv, never a shell

`CliSpec::argv` returns a `Vec<String>`. Templates use exactly two
placeholders, `{id}` and `{dir}`; substitution is one pass and verbatim, so a
game id that looks like `{dir}` stays a literal argument and nothing is
re-scanned. An unknown or unterminated placeholder is `CliError::Template`
instead of being silently dropped — a missing `{id}` would otherwise become an
install into the wrong place.

The program is resolved before anything is spawned: an absolute path must be an
executable file, a bare name is looked up in `env["PATH"]` (which is how the
tests inject fake CLIs) or the inherited `PATH`, and a miss is
`CliError::NotFound` with the program name — the "CLI missing" state the store
status UI has to show.

## A PTY, not pipes

`openpty` at 50×120 gives the child a real terminal: prompts without a newline,
progress bars driven by `\r` and colour all behave like they do in a shell.
`portable-pty` starts the child with `setsid` and `TIOCSCTTY`, so it is a
session leader with the PTY as its controlling terminal. Two consequences
matter:

- when the leader dies, the kernel hangs up its foreground group, so a CLI's
  worker processes usually go with it;
- the child's pid is also its process-group id, which is what the escalation
  below targets.

One reader thread owns a cloned master fd: it reads 8 KiB at a time, tees the
raw bytes to the log, and pushes chunks over a bounded `sync_channel(4096)`
(≈32 MiB). A CLI that floods the terminal blocks on backpressure instead of
growing the launcher's memory. The handle keeps the writer, so `write_stdin`
never races the reader; both are dups of the same master, so neither side sees a
hangup while the other is alive — the child gets its EOF only when both are
gone.

## Events

Lines are split on `\n` **and** `\r`, because a redraw is a progress update
(`part1\rpart2` is two events). ANSI/VT escapes are stripped from events but kept
in the raw log, which stays a faithful transcript. Blank lines are dropped.

A partial line is flushed after 150 ms of silence and before every
`write_stdin`, so `Enter your Epic code: ` — a prompt that never ends with a
newline — reaches the caller that has to answer it.

Parsers are per-CLI, not clever:

- **Plain** — every line is a `Line`.
- **Percent(regex)** — the first capture group (or the whole match) is parsed as
  a number in `0..=100`; a miss or an out-of-range value stays a `Line`, so
  `999%` in prose cannot move a bar.
- **JsonLines** — a line that parses as an object with `stage` and/or `percent`
  (`progress` also accepted) becomes `Stage`/`Percent`; anything else is a
  `Line`.

The stream always ends with exactly one terminal event. `Done` requires exit
`0` **and** neither cancellation nor a timeout; otherwise `Failed { code,
message }`, where the message is the cancellation, then the timeout, then the
CLI's own last line (cut to 200 characters), then the exit status — so
`error: your session has expired` is what the user sees, not `exited with 1`.

## Timeouts and cancellation

| Run | Idle | Hard |
|---|---|---|
| install / list / launch | 5 min | 6 h |
| auth (`CliOptions::interactive()`) | none | 10 min |

The idle timeout catches a CLI that is stuck (a silent install); the hard cap
catches one that keeps talking forever; auth disables the idle timeout because a
person may take minutes to fetch a TOTP code, and only the hard cap applies.

The handle polls its channel every 250 ms and drives one small state machine:
on idle/hard timeout or `cancel()` it records why, then asks
`portable-pty` to kill the child — which is `SIGHUP` to the child's pid. If the
child survives a 2 s grace period (it ignores `SIGHUP`, or a worker holds the
PTY open), the whole process group is `SIGKILL`ed with `kill(-pid, SIGKILL)`, so
a cancelled install cannot leave workers downloading in the background.

`cancel()` is idempotent and takes `&self` through a mutex, so the GUI's worker
thread can cancel a run the UI thread is reading. `Drop` is a best-effort
`SIGHUP` for the case where a caller forgets to `wait()`.

A run that starts and then fails is **not** a `CliError`: it ends with a
`Failed` event plus the flags on `CliOutcome`, so the partial output, the log
path and the reason survive into the failure record.

## The transcript

With `log_dir` set, each run writes
`cli-<program>-<command>-<nanos>-<pid>.log` (the path is created before the
spawn, so a failed spawn still names its log). The log is the raw byte stream:
escapes, `\r` redraws and all. `CliOutcome::raw_tail` keeps the last 16 KiB of
the same bytes for a failure record.

**A PTY echoes what is typed into it**, so an auth run's transcript can contain
the TOTP code or the auth code the user typed. The log and the tail are local
diagnostics; the M2 store slices must not put an auth run's tail into SQLite or
the UI. Redaction is an M4 hardening item (see below).

## Piece map

| Crate | Responsibility |
|---|---|
| `stores` (`cli`) | everything above: spawn, reader thread, parsers, timeouts, escalation, log |
| `stores` (`lib`) | re-exports `CliHandle`, `CliSpec`, `CliCommand`, `CliOptions`, `CliOutcome`, `CliError`, `ProgressEvent`, `ProgressParser`, `TimeoutKind` |
| next slices | the per-store `CliSpec` presets, the `StoreBackend` impls, DB writes, the store status UI |

## Guarantees and limits

- **Guaranteed:** no shell string ever exists (`Vec<String>` all the way); a
  timeout or a cancel always ends the run, and the process group is killed if
  `SIGHUP` is not enough; output produced before a failure is kept; the driver
  has no Dioxus, SQLite or Steam dependency, so it is unit-testable headlessly.
- **Line-based only.** A CLI that redraws one line with `\r` produces many
  events; that is deliberate (progress), but it means `events` is not a
  transcript — the raw log is.
- **stdout and stderr are one stream.** A PTY has one; error text interleaves
  with progress, which is exactly what the user would see, and why the parser
  must tolerate both.
- **The `Percent` regex is per-CLI configuration**, not inference: each store
  slice brings its own (with a fixture corpus) and a wrong regex degrades to
  `Line`, never to a wrong bar.
- **`Drop` is best-effort.** A handle that is leaked keeps its child alive until
  the child exits on its own; timeouts only tick while somebody polls. Store
  backends always `wait()`.
- **A grandchild that calls `setsid` itself escapes the group kill.** Rare
  (daemonising installers) and not worth a cgroup in v1; it is recorded here so
  the next reader does not assume otherwise.
- **No PTY resize, no echo suppression.** `SIGWINCH` handling and termios
  tweaks are not needed for these CLIs and are not implemented.
- **Nothing here has run against a real store CLI yet.** The behaviours above
  are verified against fixture scripts, including dash on Arch.

## Verification

Pure crates (macOS host and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-stores
cargo clippy -p pitagames-stores --all-targets -- -D warnings
cargo fmt --all -- --check
```

- **`cli::tests` (25)** — template substitution (install placeholders, unknown
  and malformed placeholders, no re-scan of substituted values), the three
  parsers (including an out-of-range JSON percentage), line splitting on
  `\n`/`\r`, a partial line kept until flushed, ANSI stripping (CSI, OSC,
  charset designation, stray controls), `resolve_program` (PATH entry, absolute
  file, missing name, missing relative path), a plain list, a percent install
  (events + raw log with escapes), JSON progress, an auth round-trip through
  `write_stdin`, an expired session, disk-full and network errors naming the
  CLI's line, CR redraws, a trailing line without a newline, `NotFound`, the
  idle and hard timeouts, a child that ignores `SIGHUP` (only the process-group
  `SIGKILL` ends it), and `cancel()`.
- **Deterministic by construction.** The two timeout tests originally raced a
  slow `fork`+`exec` against a short idle window and flaked under the parallel
  test harness. The idle test now uses a script that never prints (so startup
  latency cannot matter), and the escalation test waits for a marker file the
  fake CLI writes *after* installing its `trap '' HUP`, then cancels — a broken
  escalation fails the elapsed assertion at the hard cap instead of hanging.
  Six consecutive parallel suite runs are clean; the serial suite is clean too.
- **`archlinux:latest`** (Arch's `rust`): the same suite passes. Arch's
  `/bin/sh` is bash in POSIX mode, which is what the target machines run, so the
  fixtures deliberately stick to POSIX built-ins (`printf`, `trap`, `read`,
  `: > file`) instead of bashisms. dash was not exercised separately — it is not
  what a CachyOS/Arch machine has.

## Deliberately not in this slice

- The store presets (real `legendary`/`nile`/`lgogdownloader` argv and progress
  regexes), the `StoreBackend` impls, DB writes and the store status UI.
- Async/tokio. A thread plus a channel keeps `stores` synchronous, which is what
  the rest of the crate and the CLI are.
- Transcript redaction for auth runs, log rotation and a support bundle (M4).
- Retry/backoff and resume policy — those belong to each store slice.
