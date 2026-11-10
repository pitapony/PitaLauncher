# M1 — Single-instance lock

Status: implemented and verified locally (pure crates). `crates/app` compiles
the same code and is verified in `archlinux:latest`. Not yet run against a live
Steam client on the RX 9070 / RX 5700 XT.

## Why this exists

PitaGames owns one database and writes Steam's own config files
(`localconfig.vdf`, `config.vdf`). Two copies of the launcher running at once
would scan the same libraries, race over the shim profiles and fight over
Steam's config files through the guarded writers — the very corruption those
writers exist to prevent. The plan's rule is a `flock` so **two windows can't
fight over Steam config**.

## The lock (`core::lock`)

`core::lock::InstanceLock` is a framework-free, dependency-free advisory lock
built on `std::fs::File::try_lock` (stabilised in Rust **1.89**, the reason the
workspace MSRV moved up from 1.88):

```rust
let lock = InstanceLock::acquire(path)?;   // held while `lock` is alive
```

- **`flock(2)`-backed.** The kernel releases the lock when the process exits or
  is killed, so there is no stale pid file to clean up and no lock left behind
  after a crash or a `SIGKILL`. This is why a lock file (with `try_lock`) is used
  instead of `create_new(true)`, which a crash leaves permanently "held".
- **Advisory.** It protects only the code that takes it. That is on purpose: it
  is a launcher-instance guard, not a filesystem lock on the database.
- **Self-describing.** On success the holder writes its pid into the (already
  locked) file; a second process can read it back for the message, so the user
  sees *which* instance is running. Failing to write the pid is not fatal.
- **`LockError::AlreadyRunning { path, pid }`** is the ordinary "someone else is
  running" answer; `LockError::Io` is a real filesystem failure.
- Dropping the `InstanceLock` unlocks and closes the file explicitly
  (`File::unlock`), documenting the lifetime contract.

### Path

```text
~/.local/share/pitagames/pitagames.lock
```

`core::paths::lock_file()` resolves it next to the database (the resource two
instances would race over). The file is never deleted and only ever carries the
holder's pid; the lock is the `flock`, not the file's existence. The plan text
said `library.db.lock`; the database is `pitagames.db`, so the lock is named
after the app instead of the file it guards.

## Behaviour in the GUI

`crates/app/src/main.rs` takes the lock **before** the window is created and
holds it for the whole run:

- **Lock acquired** → the launcher starts normally.
- **Already running** → prints `PitaGames is already running (pid N).` and the
  lock path on stderr, then exits `0`. Re-focusing the existing window is the
  later desktop-entry/tray slice ("single-instance focus"); for now the second
  launch simply declines to run.
- **Genuine I/O failure** → prints the error and exits `1`. Continuing unlocked
  would defeat the guard.
- **No data directory** (`HOME` unset) → a warning, then the launcher runs
  without a lock. There is nowhere to put the lock file and nothing to protect.

## Why the shim and the CLI do not take it

`pitagames-shim` runs *inside* a game launch and must never be blocked by the
GUI; `pitagames-cli` is a diagnostic that is expected to run alongside the GUI.
Both are deliberately lock-free. The writers they share already carry their own
Steam-closed guard, backups and journals, so a concurrent CLI write is still
safe.

## Guarantees and limits

- **Guaranteed:** a second GUI instance does not start; the lock is released on
  exit and on crash; the holder's pid is visible to the next process; the lock
  file's parent directory is created.
- **Lock-scope only.** The lock is advisory and process-scoped. It does not stop
  a *different* program (or a stale build) from editing the same files, and the
  CLI/shim bypass it on purpose.
- **No focus-forwarding yet.** The second launch reports and exits; it does not
  raise the running window. That is the M1 `.desktop`/tray slice.
- **Not hardware-tested.** No live Steam client on the RX 9070 / RX 5700 XT has
  raced two instances yet.

## Verification

- `pitagames-core`: **55 tests** (was 51), including 4 `lock` tests (acquire and
  path, a second acquire reports `AlreadyRunning` with the holder's pid, drop
  releases the lock, missing parent directories are created).
- Pure crates fmt + clippy `-D warnings` + tests clean locally
  (core 55, db 22, art 14, metadata 28, platform 29, stores 98 unit + 13
  integration, secrets 9, shim 8, cli 19).
- `crates/app` builds, clippy-clean and **46 tests** pass in `archlinux:latest`
  (unchanged count; the startup path takes the lock in the same binary CI
  lints).
