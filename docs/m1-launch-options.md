# M1 — Steam launch-options writer

Status: implemented and verified against fixtures + CLI smoke runs. The GUI
Proton picker that consumes it now ships (see `docs/m1-profile.md`).
**Not yet exercised against a live Steam client on the RX 9070 / RX 5700 XT.**

## Why this exists

Steam's per-game launch options are how PitaGames routes a game through the
shim:

```text
DXVK_HUD=fps %command% -novid
        ↓
DXVK_HUD=fps pitagames-shim 620 %command% -novid
```

`localconfig.vdf` is Steam's file. It contains far more than launch options —
playtime, last-played timestamps, cloud settings, per-app flags — and Steam
rewrites it on exit. A writer that re-serialises the whole document would lose
comments, ordering and any key Steam or the user added. This slice is therefore
a **byte-level splice**, wrapped in the guard rails from the plan:

1. refuse while Steam is running;
2. change only the requested app's `LaunchOptions`;
3. keep a timestamped backup;
4. replace atomically (temp file, `fsync`, `rename`);
5. restore the original bytes when the file is unchanged.

## Modules

| Layer | File | Responsibility |
|---|---|---|
| Format-preserving edit | `crates/stores/src/localconfig.rs` | Span-aware VDF scanner; reads one app's value and splices a single key/value pair into the original bytes. |
| Guarded write/restore | `crates/stores/src/steam_config.rs` | Steam-closed check, backup, restore journal, atomic replace, byte-identical restore. |
| Shim string helper | `crates/core/src/launch.rs` | `shim_launch_options(appid, existing)` / `strip_shim_launch_options`. |
| CLI diagnostic | `crates/cli/src/main.rs` | `pitagames-cli launch-options …`. |

Both store modules are framework-free and DB-unaware. The GUI can call the same
functions without a Dioxus dependency.

## Format-preserving edit

`localconfig::read(input, app_id)` walks
`UserLocalConfigStore → Software → Valve → Steam → apps → <appid> →
LaunchOptions` and returns the decoded string. A missing section/app/key is
`Ok(None)`; only the writer insists on the expected structure.

`localconfig::edit(input, app_id, Some(value))`:

- parses the whole document first with the public VDF parser (a broken file is
  never edited);
- replaces only the quoted value span when the key exists;
- inserts a `LaunchOptions` pair when the app object exists but the key does not;
- inserts a whole app object when the app does not exist;
- re-parses the result before returning it.

`None` removes the key. An app object that only ever held the launch options is
removed with it, so set-then-clear returns the original layout. Indentation is
copied from the surrounding object (`\t` inside multiline objects, a space in
inline ones). Comments and `[$CONDITIONAL]` suffixes survive because no other
byte is touched.

The three fixtures live in `fixtures/steam/`:

- `localconfig_empty.vdf` — no app entries;
- `localconfig_with_options.vdf` — an existing `-novid %command%` plus playtime;
- `localconfig_quoting.vdf` — nested `WINEDLLOVERRIDES="dxgi=n,b"` quoting and a
  Windows-style quoted executable path.

## Guarded write

`steam_config::apply_launch_options(localconfig, app_id, value, steam_state,
options)`:

1. **Refuse while Steam runs.** `detect_steam_process(steam_root)` checks
   `<root>/steam.pid` against `/proc/<pid>`, then scans `/proc/*/comm` for an
   exact `steam` process (not `steamwebhelper`). A stale pid file is ignored.
   On non-Linux the state is `Unknown`, so a write needs an explicit override.
2. **Read and edit without writing.** The whole file must be UTF-8; a binary
   config is refused rather than guessed at.
3. **No-op if the value is already correct.** Nothing is backed up and no file
   mtime changes.
4. **Keep one restore point per app, user and config file.** The first write
   stores the pre-PitaGames value and a copy of the original file. Repeated
   writes keep that same restore point instead of treating the already-shimmed
   line as original. A journal belonging to a different Steam root is never
   overwritten; the write fails and asks for a separate backup directory.
5. **Write the journal first.** `<backup_dir>/localconfig-<user>-<app>.json`
   records the app, user, file, backup path, previous/applied values and the
   SHA-256 of both the original and written documents. A backup without a record
   is worse than a record without a write.
6. **Atomic replace.** A sibling `.<name>.tmp-<pid>-<timestamp>` file is written,
   `fsync`ed, given the original file's permissions and `rename`d over the
   config. The parent directory is `fsync`ed best-effort. A failed write removes
   the temp file and the journal.
7. Return `WriteReport { previous, applied, changed, backup, user_id }`.

Backups default to `$XDG_DATA_HOME/pitagames/backups/steam` and are named
`localconfig-<user>-<app>-<unix-nanos>-<pid>.vdf`.

## Restore

`steam_config::restore_launch_options` reads the journal and compares the
current file's SHA-256 with `written_sha256`:

- **Exact restore** — the file is still exactly what PitaGames wrote. The
  original backup is copied back through the same atomic writer; the result is
  **byte-identical** to the pre-write file. The journal is removed.
- **Merged restore** — Steam changed something else since the write (playtime,
  another app, a settings toggle). The whole-file copy would clobber those
  changes, so only this app's `LaunchOptions` is spliced back. A
  `…-pre-restore.vdf` copy of the current file is kept first. The report says
  `exact: false`.
- A tampered or missing backup is never restored; the restore fails with
  `BackupModified`.
- A missing journal is a clear `NoRecord` error naming the app and path.

The journal is deliberately stored next to the backup rather than only in
SQLite, so `pitagames-cli launch-options --restore` works on a machine where the
GUI has never started. The GUI will mirror the fields into SQLite when it owns
the apply flow.

## CLI

```bash
# Read the current value (also prints the resolved user and Steam state).
pitagames-cli launch-options 620

# Preview without writing.
pitagames-cli launch-options 620 --shim --dry-run

# Route the game through the shim (preserves an existing prefix/suffix).
pitagames-cli launch-options 620 --shim

# Set a literal value, or clear the key.
pitagames-cli launch-options 620 --set 'DXVK_HUD=fps %command% -novid'
pitagames-cli launch-options 620 --clear

# Put the original value back.
pitagames-cli launch-options 620 --restore

# Fixture/other-install diagnostics.
pitagames-cli launch-options 620 --localconfig /path/to/localconfig.vdf \
  --backup-dir /tmp/backups --force
```

Target resolution:

- `--localconfig <path>` edits that file directly and derives the Steam root by
  walking up to the `userdata` ancestor;
- otherwise `--steam-root` (or auto-detection) + `--user` (or the only user with
  a config) resolves `<root>/userdata/<id>/config/localconfig.vdf`.

`--force` is required when Steam is running or when the platform cannot inspect
processes (macOS development runs). `--json` prints the same fields as the text
output.

## Guarantees and limits

- **Guaranteed:** only the target app's `LaunchOptions` changes; the file is
  valid VDF after every edit; writes are atomic; a backup always exists before
  the replace; an unchanged file restores byte-for-byte; a repeat write refuses
  if the original restore-point backup disappeared or changed.
- **One restore point per app, user and config file:** the value is the one from
  before the first PitaGames write. If the user edits launch options in Steam
  afterwards, `--restore` returns to the PitaGames original, not the user's
  intermediate value. A journal from another Steam root is refused, not
  overwritten.
- **Merged restores cannot be byte-identical** when Steam changed other keys in
  between; they preserve Steam's changes and restore only the launch options.
- **Steam detection is Linux-only.** `/proc` and `steam.pid` are the evidence;
  macOS returns `Unknown` and requires `--force`.
- **No compat-tool write yet.** Forcing a Proton build per game lives in
  `config.vdf` `CompatToolMapping`; the plan defers it to the same guard rails
  and it is not part of this slice.
- **GUI consumer ships** (`docs/m1-profile.md`): the detail page's Runtime panel
  applies and restores this writer through `crates/app/src/runtime.rs`, and the
  shim profile it writes is what `pitagames-shim` reads at launch.
- **Hardware untested.** Fixtures and CLI smoke runs prove the byte behaviour;
  a real Steam client on the RX 9070 / RX 5700 XT has not run it yet.

## Verification

- `pitagames-core`: 4 new tests for shim composition — empty/existing prefixes,
  idempotent double application, shim replacement (never nesting), stripping.
- `pitagames-stores`: 14 `localconfig` tests (read/quote/missing key/removal/
  inline layouts/refusals/comments) and 21 `steam_config` tests (byte-identical
  round trip over all three fixtures, Steam-changed merge, running/unknown
  refusal, force override, repeated-write restore point, remove-key restore,
  no-op write, no record, tampered/missing backup, no temp files, non-UTF-8,
  journal naming, journal collision across roots, corrupt journal, pid-file and
  `/proc` detection, stale pid, unusual user paths, SHA-256).
- `pitagames-cli`: 6 new tests for argument parsing, action conflicts, Steam-root
  derivation and user picking.
- CLI smoke run on a fixture tree: read → `--shim --dry-run` → `--shim` →
  `--restore`; the restored file compares byte-identical with the original, and
  the backup directory contains one `.vdf` plus one `.json` journal.
- `crates/app` still builds, passes clippy `-D warnings` and its 29 tests in
  `archlinux:latest` (no GUI consumer yet).
