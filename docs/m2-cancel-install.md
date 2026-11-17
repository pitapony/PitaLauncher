# M2 — cancelling a running install (`stores::installer::InstallControl`, `app::store_installs`)

Status: implemented. Verified with fake store CLIs on macOS and in
`archlinux:latest`, plus an Xvfb run in the container where a fake `legendary`
downloading from a grid card was stopped from the Downloads board and the row
read `cancelled`. Not driven against a real `legendary` / `nile` / `gogdl`.

## Why this slice exists

The GUI install flow (`docs/m2-store-installs.md`) could start an install but not
stop one: `Installs::cancel` refused a `Running` job, and the doc listed "a
running install cannot be cancelled from the UI" under known limits. The reason
was structural — `installer::run` called the backend's blocking `stream`, which
spawns the PTY `CliHandle` internally, so the UI thread had no reference to
the process it wanted to signal.

The CLI driver had the other half of the problem solved two slices earlier: the
auth terminal made `CliHandle::cancel(&self)` usable from a second thread by
splitting the state lock from the reader channel
(`docs/m2-auth-terminal.md`). This slice connects the two.

## `stores::installer` — the control

`run` now spawns the CLI itself instead of calling a backend's `stream`, and
registers the handle with an `InstallControl` the caller holds:

| Item | Role |
|---|---|
| `InstallControl` | `Clone` handle (`Arc` inside): `cancel()`, `is_cancelled()` |
| `InstallControl::attach` (private) | publishes the `Arc<CliHandle>` for as long as `run` drives it |
| `run(backends, job, options, control, on_progress)` | the blocking half; now cancellation-aware |
| `InstallRun::cancelled` | the run was stopped on request (not a failure) |

`cancel()` is safe at every moment:

- **Before the CLI starts** it only sets the flag, and `run` returns a cancelled
  `InstallRun` without spawning anything — no process, no directory, no
  transcript.
- **While the CLI starts** the flag and the registration race safely:
  `attach` returns whether a cancel already arrived, and `run` then cancels the
  handle it just registered.
- **While the CLI runs** it signals the process group through
  `CliHandle::cancel` — `SIGHUP`, then `SIGKILL` after the driver's 2 s grace.
- **After the run** the handle is detached, so a late click cannot resurrect a
  process; the flag stays readable.

The dispatch is `store → backend.spawn` / `store → backend.report`, which keeps
two things that already existed in the backends: the argv (`CliSpec`) and the
log layout each CLI writes (`Legendary` for legendary, `Nile` for nile,
`Gogdl` for gogdl — see `stores::report`). `run` still refuses Steam/Local
before spawning.

A cancelled run is `Ok`, not `Err`: the user asked for it, and quoting the
driver's signal death as a store failure would be a lie. Whatever the CLI had
written into the target directory stays; the store's own repair/verify is the
way to finish it.

## `app::store_installs` — one board, two new rules

**Reports are keyed by `GameId`, not by row index.** The queue is a `Vec` and a
finished row can be removed above a running one, which shifts every index after
it. The old `InstallEvent { index, .. }` would then land a progress update on
the wrong job — or on none, leaving the running row stuck forever. `GameId` is
stable, and the queue refuses two active jobs for the same game, so it identifies
the worker's job uniquely. `InstallEvent` is now:

```text
InstallEvent::Progress { id, progress }
InstallEvent::Finished { id, outcome: InstallOutcome }
InstallOutcome::{Done, Cancelled, Failed(String)}
```

**A cancelled run keeps its slot until the worker confirms.** `Installs` keeps
one `InstallControl` per started job (`controls: BTreeMap<GameId, ..>`) and a set
of jobs whose CLI has been asked to stop (`cancelling: BTreeSet<GameId>`).
`Installs::cancel` on a `Running` job calls the control and records it as
cancelling, but leaves the queue state `Running`: the CLI is still shutting down,
and the serial queue exists because two store clients fight over one login
session and one `~/.config` cache. The row therefore reads `cancelling` (the
grid card's label does too) until the worker sends `Finished { Cancelled }`,
which marks the row `Cancelled` and lets `pump_installs` start the next job.

`Installs::control(index)` is created by `pump_installs` **before** the worker
thread starts, so a Cancel click during the CLI's startup reaches it; the worker
gets the clone and `spawn(job, sender, control)` no longer takes an index.

## The GUI side

The Downloads board's row shows **Cancel** while a job is queued *or* running
(and no button while it is already cancelling), the state word reads
`cancelling` in between, and the finished row reads `cancelled` with Retry and
Remove — the same affordances a failed job has. A cancelled install does not
flip the grid card to installed and writes nothing to SQLite.

## Verification

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-art -p pitagames-metadata \
  -p pitagames-platform -p pitagames-stores -p pitagames-secrets \
  -p pitagames-shim -p pitagames-cli
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

In `archlinux:latest` (`CARGO_TARGET_DIR=/target`): `cargo test -p pitagames`,
`cargo clippy -p pitagames --all-targets`, `cargo fmt --all -- --check`.

- `stores::installer` **11 → 14 tests**: cancelling a running install stops the
  CLI (a fake `legendary` writes a ready marker, the test cancels once it is
  alive, and the run returns `cancelled`); cancelling before the run starts
  spawns nothing and returns no transcript; a finished run forgets its handle, so
  a later cancel only records the flag. `stores` **258 → 261**.
- `crates/app` `store_installs` **7 → 10 tests**: a running job keeps its slot
  and reads `cancelling` until `Finished { Cancelled }` arrives, then the next
  queued job starts; a running job with no registered control is not cancellable;
  reports are keyed by game (a removed row's late report is ignored, and a
  shifted row still receives its own). `crates/app` **112 → 115**.
- Xvfb smoke run with a fake `legendary` on `PATH` and a synced Epic row:
  clicking **Install** on the Gizmo card started the fake, whose progress reached
  `5%`; the Downloads row showed **Cancel**, clicking it flipped the state word
  to **CANCELLING**, the fake wrote its `killed` marker, and a moment later the
  row read `cancelled` with Retry/Remove and the nav count dropped back to 0. The
  run's transcript under `<data>/pitagames/logs/cli-legendary-install-*.log` ends
  at the last progress line, the library row stayed `installed = 0`, and no
  store CLI process was left behind.

## Known limits

- **Not run against a real store CLI.** The signal path is the driver's, which
  the CLI already exercised; the verdicts and progress shapes come from the
  same fixtures the backends use.
- **The CLI's own cleanup is its business.** A killed `legendary`/`nile`/`gogdl`
  may leave a partial download in the target directory. Cancelling is not an
  uninstall; the row stays `cancelled` and the store's `verify`/repair (or a
  Retry into the same directory) is the way forward. Removing the partial files
  is not offered yet.
- **Cancel is cooperative.** The driver sends `SIGHUP` and escalates to
  `SIGKILL` after a 2 s grace; a CLI that ignores both would keep the row
  `cancelling` until the group dies. Nothing times the shutdown out on the UI
  side.
- **The queue still does not survive a restart**, so a job cancelled by closing
  the window has no record; its CLI is killed by `CliHandle::drop` because the
  worker thread dies with the process.
- **A cancelled job's row is removed only by the user** (`Remove`) or
  `Clear finished`. There is no automatic cleanup.

## Deliberately not in this slice

- A partial-download cleanup or "cancel and delete files" action.
- Persisting the queue (and a cancelled-run history) across restarts.
- Pausing/resuming a store download (the CLIs do not expose it).
