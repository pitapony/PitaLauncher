# M4 — crash recovery: the orphaned install, the surviving game, the persisted queue

Status: the install queue persists in SQLite, a `Running` job left by a crashed
launcher surfaces as `Failed` with **Retry**/**Verify**, and an orphaned store
CLI's pid is reaped at the next boot behind an identity check. Unit-tested over
in-memory SQLite and fake CLIs on macOS, and built/linted in `archlinux:latest`.
**Not driven against a real store CLI** — no target machine has been reachable.

## Why this slice exists

Two different things happen when PitaGames dies mid-work, and they must not be
confused:

- **A running game lives.** `platform::launch` starts every supervised game in
  its own session (`setsid`), so the game is not a child of the launcher and a
  launcher crash does not signal it. That is deliberate and unchanged.
- **A running install is orphaned.** A store install is the opposite: the
  launcher runs `legendary`/`nile`/`gogdl` on a PTY, so those CLIs *are* its
  children. When the launcher dies, the CLI keeps holding the store's
  `~/.config` session lock, half-writing files.

Before this slice the queue existed only in memory: on the next start a crashed
install was simply forgotten, and the orphaned CLI was never cleaned up. The
rules are:

- **A partial download is never deleted.** Whatever an interrupted store wrote is
  the store's to reconcile, through its own `verify`/`repair`. The launcher only
  offers it.
- **A pid is not a durable handle.** After a crash — or a reboot — a pid can be
  recycled. The recorded pid is signalled only when it still names the store's
  own CLI.
- **The queue is a snapshot, not a log.** Each write names the whole queue, so
  order, deletes and edits are one transaction and cannot drift.

## The persisted queue

`crates/db/migrations/0005_install_queue.sql` adds one table; the queue layer owns
its meaning.

| Item | Role |
|---|---|
| `db::StoredJob` | one persisted row: id, title, target, size, kind, state, message, pid, progress |
| `db::QueueRepository` | `replace_queue` (one transaction), `stored_queue`, `clear_queue` |
| `queue::JobKind::wire`/`from_wire` | the stable token persisted for a job kind |
| `queue::JobState::wire`/`from_wire` | the same for a state (the failure message rides its own column) |
| `queue::InstallJob::pid` | the running CLI's process-group id, recorded while it is alive |
| `queue::InstallQueue::{from_jobs, snapshot, reconcile_running, set_pid, verify}` | the pure queue transitions |
| `app::store_installs::{stored_job, install_job}` | the `InstallJob ↔ StoredJob` mapping (skips an unknown token rather than guessing) |

The app writes the queue with a full replace on every crash-critical transition:
a job is persisted the moment it is enqueued, a running job's pid the moment its
worker reports it (`InstallEvent::Started`), and the final state when it
finishes. Progress ticks are deliberately not persisted — a database write per
percent buys nothing.

At startup `pumps::bootstrap` reads the queue back, rebuilds the board with
`Installs::restore`, and **reconciles**: every `Running` job becomes
`Failed { "an earlier launcher run was interrupted" }` and hands its pid back for
reaping. A `Queued` job that never started stays queued (and is re-persisted as
such); a finished job is untouched. The reconciled queue is written back, so a
second restart sees failures, never phantom running jobs.

## Reaping the orphan — identity-checked

`crates/stores/src/recovery.rs` is the decision, kept pure and injectable:

```text
  Interrupted { id, pid }  ─►  recovery::reap  ─►  OrphanOutcome
                                   │                  ├─ Signalled { pid, program }
                                   │                  ├─ Gone { pid }
                                   └─ Signalled only   └─ Foreign { pid, command }
                                      when /proc/<pid>/cmdline still names the
                                      store's own CLI (legendary/nile/gogdl)
```

- The process table and the killer are both injected, so the whole decision is
  testable without spawning anything. The app passes `recovery::system_cmdline`
  (reading `/proc/<pid>/cmdline` on Linux; `None` elsewhere) and
  `platform::launch::terminate` — the same `SIGTERM`→`SIGKILL` escalation a Stop
  button uses.
- `legendary` and `nile` are Python scripts, so the pid's argv is
  `["python3", "/usr/bin/legendary", …]`; the match is on the **file name** of
  any argument, not on argv[0].
- A pid that is gone, or that now belongs to another program, is left exactly
  where it is. A launcher must never kill a stranger.

## Verify: the store's own repair

An interrupted install is offered a **Verify**, not a delete. `JobKind::Verify`
is a third kind that runs the backend's `verify`/`repair` **alone** — the same
`installer::run` and the same verdicts as an update's verify, with no install
phase. `installer::verify_plan(&Game)` is the plan; `InstallQueue::verify(index)`
turns a finished row into one, keeping the directory it was filling.

- In the GUI the Downloads board's failed row shows both **Retry** and
  **Verify** (and the pad's `Confirm` on a failed row is Retry, as before).
- `pitagames-cli stores verify --store <name> <id> [--dir <dir>] [--dry-run]`
  is the headless run.

## Wiring

| Where | What |
|---|---|
| `stores::cli::CliHandle::pid` | the child's process-group id (is also the group `cancel` signals) |
| `stores::installer::run` | gains an `on_started(pid)` callback, fired before `control.attach` so the pid can be persisted before the CLI can be orphaned |
| `stores::installer::program_for` | made public: crash recovery asks the same "which CLI is this?" |
| `app::store_installs::InstallEvent::Started` | the pid report the worker sends |
| `app::store_installs::{restore, stored, reconcile, started, verify, reap_orphans}` | the board's crash-recovery surface |
| `app::pumps::persist_queue` | the one full-replace write |
| `app::pumps::bootstrap` | restore → reconcile → reap → re-persist |
| `app::launch_start::start_install` | persists the queue the moment a job is enqueued |

## Acceptance

| Criterion | Where it holds |
|---|---|
| Killing the launcher leaves a running game alive and the session reconcilable | games are `setsid`'d (`platform::launch`, unchanged); the *install* session is reconciled from `install_queue` |
| An orphaned install's pid is reaped at next boot | `reconcile_running` → `Instances`/`recovery::reap` → `platform::launch::terminate` |
| A persisted `Running` job surfaces as `Failed` with Retry/Verify | `reconcile_running` + the Downloads board's Retry/Verify buttons |
| No partial download is removed by the launcher | no path but the target directory is stored; only the store's own verify reads it |

## Tests

- `stores::queue` (7 new): the kind/state wire round-trip, `from_jobs`/`snapshot`,
  `reconcile_running` turning a `Running` job into `Failed` and handing back its
  pid, a pid-less running job, `set_pid` attaching only to a running job, a stale
  pid cleared on start, and `verify` requeueing a finished job as a verify-only
  repair.
- `stores::recovery` (6): a pid still naming the CLI is signalled; a recycled pid
  of another program is left alone; a gone pid is not killed; a store with no CLI
  program is never signalled; only pids recorded by running jobs become orphans;
  `reap_all` kills only the matching one.
- `stores::installer` (4 new): a verify-only job checks without installing; a
  failed verify-only job reads "verify failed", not "update verify"; the runner
  reports the CLI pid before it can be orphaned; `verify_plan` keeps the existing
  directory and refuses a store with no installer.
- `pitagames-db` (5 new, in-memory SQLite): the queue round-trips in order (pid,
  size, message, progress included); replacing drops omitted rows; `clear_queue`;
  a corrupt row is reported not guessed; a negative pid is reported.
- `app::store_installs` (6 new): the `InstallJob ↔ StoredJob` round-trip for
  install/update and a failure; an unknown token is skipped; a restored running
  job surfaces as failed and reports its pid; `verify` requeues a failed row; a
  `Started` event attaches the pid only to a running job.
- `pitagames-cli` (1 new): `stores verify` repairs without installing.

## Known limits

- The reaper reaps only on Linux (`/proc`); everywhere else `system_cmdline` is
  `None` and every pid reads as `Gone`, so nothing is signalled. The target is
  Linux only.
- A queued job is persisted at enqueue and on the running job's next
  Started/Finished; a job queued while another is silent and then a crash loses
  that queued row (it never started, so nothing is orphaned or half-written).
- The recorded pid is the PTY session leader's; a store CLI that daemonises into a
  *new* session would not be caught by a group signal — none of the three does.
- Progress is not persisted between transitions, so a restored row shows the bar
  it had at its last transition, not its last percent.
- The Downloads **Verify** is the pointer button and a CLI command; the pad's
  finished-row `Confirm` is still Retry only.
- Not driven against a real interrupted install or a real orphaned CLI.
