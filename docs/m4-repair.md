# M4 — repair installation and the local settings export

Status: `pitagames-repair` (the `pitagames-repair` crate), `pitagames-cli
repair` / `repair export` and the Settings screen are implemented and
unit-tested against `tempfile` trees and fixture databases on macOS and in the
`archlinux:latest` container. **Not driven against a real modded game, a real
Steam install or a real GPU** — the same hardware gate as the rest of M4
(ADR-0004).

## Why this slice exists

When a launcher misbehaves the user wants one action that says what is wrong and
puts right what can be put right, not three diagnostics in three places. This
slice is that action. It also answers the privacy question from the plan: a user
who wants to show a maintainer their machine can **copy or save** a support blob
with no endpoint to send it to and no account data in it.

Three things can be wrong, and each is the responsibility of a different crate:

| Symptom | Owner | What repair does |
|---|---|---|
| Steam "verify integrity" reverted a modded file | `crates/mods` | re-hash every manifest; report a tampered tree; `reapply` it |
| A launch option or forced Proton is still set | `crates/stores::steam_config` | list/apply the journals the launcher wrote |
| The library database is damaged or a migration is behind | `crates/db` | back the file up before migrating; `PRAGMA integrity_check` |

## One repair, framework-free

`crates/repair` is the new framework-free crate both surfaces drive, so the
CLI's `repair` command and the Settings button cannot drift.

- **`mods`** — `find_manifests(root)` walks a game root (bounded to
  `SCAN_DEPTH`) and never descends into a `.gl-backup/` directory;
  `check_roots(&[root])` verifies every manifest and reports `Clean`,
  `Tampered { issues }` or `Unreadable { message }`. A tampered tree is not a
  dead end: `reapply(dir, mod_name, sources)` restores the tree to its
  pre-install state and installs the mod again from the caller's sources, so the
  recorded hashes come back. `restore(dir)` is the safe half when only an
  uninstall is wanted.
- **`steam`** — `RepairRequest.steam_backup` points at
  `<data>/pitagames/backups/steam`; repair lists (`Pending`) or applies
  (`Restored`) every journal there through
  `stores::steam_config::{pending_restores, restore_pending}`, which reuse the
  CLI's own guarded restore — byte-for-byte where Steam has not changed the file,
  merged where it has.
- **`db`** — `run` opens the database with `Db::open`, which takes a
  `VACUUM INTO` snapshot beside the file whenever a migration is pending on an
  existing database; it then calls `Db::integrity_check` (SQLite's own
  `PRAGMA integrity_check`).

`RepairReport::is_clean()` answers whether anything needed attention, and
`tampered_mods()` names what a re-apply is offered for. The report is
`Serialize`, so both surfaces can print JSON.

## The local settings export

`repair::export` renders a support blob with **no endpoint and no secrets**:

- the detected GPU (`platform::gpu`),
- the Proton builds (`platform::proton`) with the selected one marked,
- the selected game's resolved shim environment (`ShimProfile::env`).

Two rules are enforced in code, not documented and hoped for. There is no
network call anywhere in the module, and an environment variable whose name
contains `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `CREDENTIAL` or `AUTH` is
**dropped** rather than printed, so a stray credential cannot leak into a bug
report. A unit test plants four secret-looking variables and asserts neither
their names nor their values appear.

## Surfaces

- `pitagames-cli repair [--game-dir <dir>]... [--steam-backup <dir>]
  [--steam-root <dir>] [--db <path>] [--force] [--apply] [--json]` — checks by
  default and lists pending Steam restores; `--apply` writes them; exits
  non-zero when a mod tree is still tampered or the database failed its check.
- `pitagames-cli repair export [<store> <id>] [--dir <profiles>] [--out <file>]
  [--json]` — prints, or saves with `--out`, the local settings export.

The GUI half ships on ticket #20's `View::Settings` screen: the **Maintenance**
section at the foot of `crates/app/src/settings.rs` adds three controls to the
screen's closed `Control` set — `Control::RunRepair`,
`Control::ExportSettings` and `Control::SaveExport` — whose
`Intent::{RunRepair, Export, SaveExport}` change **no** config field (`apply()`
returns them without mutating `AppConfig`; a headless test asserts it, and that
the two new rows keep the built `FocusGraph` trap-free). The section drives the
same two entry points the CLI calls, so the button and `pitagames-cli repair`
cannot drift:

- **Run repair** — `RepairRequest::for_this_machine(true)` then
  `repair::run(&request).await` on a worker (Dioxus `spawn`), reporting
  `RepairReport::is_clean()`, the tampered-mod count (`tampered_mods()`), the
  Steam restore count (`report.steam.count()`) and the database verdict into the
  screen's status line. A `RepairError` becomes a red status; nothing panics.
- **Export settings** — `export::for_machine(selected, None)`, where `selected`
  is the library's current detail selection (else `None`); its `.text()` is
  shown in a selectable box beside the shared `crate::copy::CopyButton`.
- **Save export** — the same text written to
  `<data_dir>/pitagames/support/pitagames-settings-<unix_secs>.txt`, whose path
  lands in the status line; a write error is a red status. No network call, and
  the crate's own credential filter is what keeps secrets out.

The CLI calls both, so the screen and the command cannot drift.

## Verification

- `pitagames-db`: the integrity check on a healthy database, a damaged one
  (built with `PRAGMA writable_schema`), a pending migration that leaves a
  pre-migration backup carrying the data, and an up-to-date database that leaves
  none.
- `pitagames-stores`: `pending_restores` lists both kinds and skips a corrupt
  journal; `restore_pending` puts a clean record back byte-for-byte and repairs
  a file Steam touched.
- `pitagames-repair`: a clean install checks clean; a tampered tree is detected
  and `reapply` restores the recorded hashes; a tampered backup refuses the
  re-apply; one `run` reports all three sections and its `apply` restores the
  Steam file byte-for-byte; the export names the GPU/Proton/env and never a
  credential.

## Known limits

- **Re-apply needs sources.** A manifest records hashes, not the mod's bytes, so
  the repair detects and *offers* a re-apply; it is the mod installer (Tier B's
  pinned release, the NR installer) that supplies the files and calls
  `reapply`. The repair never fetches, so it stays offline.
- Not run against a real modded game, a real Steam install or a real GPU.
