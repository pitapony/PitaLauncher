# M1 — Single-instance lock

Status: implemented and verified in `archlinux:latest` (build, clippy, tests and an
Xvfb smoke run of two launches). Not yet run against a live Steam client on the
RX 9070 / RX 5700 XT.

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
  lock path on stderr, asks the running instance to come to the front through the
  activation socket, and exits `0`. The second launch never opens a window.
- **Genuine I/O failure** → prints the error and exits `1`. Continuing unlocked
  would defeat the guard.
- **No data directory** (`HOME` unset) → a warning, then the launcher runs
  without a lock. There is nowhere to put the lock file and nothing to protect.

## Focus forwarding (`core::activation`)

The lock answers "is someone else running?"; the activation socket answers
"tell that someone to come forward". `main` binds it immediately after taking
the lock — so only the owner ever listens — and the second launch calls
`request_focus` before exiting. One line in, one acknowledgement out (`focus` →
`ok`), on `<XDG_DATA_HOME>/pitagames/instance.sock` (`0600`). A stale socket left
by a crash is probed and removed; a path longer than 100 bytes disables
forwarding with a warning rather than failing to bind.

`docs/m1-desktop-integration.md` has the protocol, the event-loop wiring and the
Wayland caveat (the compositor may refuse focus even though the window is
raised). `pitagames-cli activate` is the client-side diagnostic.

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
- **Focus forwarding is best effort.** The socket request is delivered and
  acknowledged, but on Wayland the compositor may refuse to give the raised
  window keyboard focus. Without a socket (an older build, or a long
  `XDG_DATA_HOME`) the second launch still exits `0` after reporting why.
- **Not hardware-tested.** No live Steam client on the RX 9070 / RX 5700 XT has
  raced two instances yet.

## Verification

- `pitagames-core`: **81 tests**, including the 4 `lock` tests (acquire and path,
  a second acquire reports `AlreadyRunning` with the holder's pid, drop releases
  the lock, missing parent directories are created) and the 7 `activation` tests
  (round trip, unknown request, second bind, stale socket, path limit, no
  listener, silent client).
- Pure crates fmt + clippy `-D warnings` + tests clean locally (core 81, db 22,
  art 14, metadata 28, platform 69, stores 98 unit + 13 integration, secrets 9,
  shim 8, cli 29).
- `crates/app`: builds, clippy-clean and **73 tests** pass in `archlinux:latest`
  (66 before this slice; the new ones cover the icon painter and the focus
  channel).
- **Xvfb smoke run** in `archlinux:latest` with an empty Steam library: the first
  launch opened the window and held the lock; a second `pitagames` printed
  `PitaGames is already running (pid N).`, the lock path and
  `… asked it to come to the front.`, then exited **0**; the running GUI logged
  `[pitagames] raised the window for a second launch`. `pitagames-cli activate`
  exited **0** with `{"activated": true, "socket": …}`.
