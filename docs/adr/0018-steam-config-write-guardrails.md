# ADR-0018: Steam config write guard rails

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §5 (Steam config writes); `AGENTS.md` (Steam config writes)

## Context

Writing Steam's `localconfig.vdf` (launch options) and `config.vdf`
(`CompatToolMapping`) is the highest-risk code in the project. A bad write
corrupts a user's Steam configuration, and Steam itself rewrites these files
while running.

## Decision

Every Steam config write goes through the same guard rails
(`stores::vdf_edit` span splices + `stores::steam_config`):

- **Require Steam closed.** Detect it via `steam.pid` and `/proc/*/comm`; refuse
  and offer "Quit Steam and apply". Non-Linux is `Unknown` and needs an explicit
  override (`--force` on the CLI).
- **Atomic replace:** write to a sibling `.<name>.tmp.<pid>.<ts>`, `fsync`,
  rename.
- **Timestamped backup** kept in `<data>/pitagames/backups/steam`
  (`localconfig-<user>-<app>-<ts>-<pid>.vdf`).
- **One restore point per app/user/config file** (`localconfig-<user>-<app>.json`
  next to the backup): repeated applies keep the *original* value, not the
  already-shimmed line; a journal from a different Steam root is refused rather
  than overwritten. Write the journal before the replace, remove it after a
  successful exact restore.
- **Exact-or-merged restore:** copy the backup back byte-for-byte when the file
  still matches what we wrote; if Steam changed other keys since, splice **only
  this app's** options back (keeping a `-pre-restore.vdf` copy). A tampered backup
  is refused.
- **Never touch other keys.** Read, parse, verify the expected structure, splice
  only the target app's mapping/`LaunchOptions` — comments, conditionals and every
  other byte survive.
- **Byte-identical round-trip tests are mandatory** for `localconfig.vdf` (three
  fixtures: empty, with existing options, nested quoting). `pitagames-cli
  launch-options` and `compat-tool` are the diagnostics. Reading the same files
  (playtime/last-played) is side-effect free.

## Consequences

- Positive: the riskiest code path has one implementation, one backup scheme and
  round-trip tests, and it cannot clobber a concurrent Steam change.
- Negative: writes require Steam to be closed (a deliberate friction), and the
  restore machinery is genuinely more code than a naive write.
- The writer refuses while Steam runs; on macOS `Unknown` requires `--force` with
  explicit paths, which is expected.

## Alternatives considered

- **Write while Steam runs.** Rejected — Steam rewrites the files; last writer
  wins and can corrupt state.
- **Naive overwrite with no backup/journal.** Rejected — no recovery and no
  protection against clobbering Steam's own edits.

## References

- plan §5 *Steam config writes (highest-risk code path)*.
- `AGENTS.md` — *Steam config writes*.
- `docs/m1-launch-options.md`, `docs/m1-compat-tool.md`; `crates/stores/src/vdf_edit.rs`,
  `crates/stores/src/localconfig.rs`, `crates/stores/src/compat_tool.rs`,
  `crates/stores/src/steam_config.rs`.
