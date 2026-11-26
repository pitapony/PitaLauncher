# ADR-0030: Updates ride the install queue; Proton/GE updates are opt-in and pinned

- **Status:** Accepted
- **Date:** 2026-11-25
- **Deciders:** julienpohie
- **Source:** spec #8 (M4 *Updates*), ticket #34; `docs/m4-updates.md`; plan §7/§8;
  extends ADR-0015 (the install queue), ADR-0019 (supervision) and ADR-0024
  (pinned releases)

## Context

By the end of M2 every CLI store had an installer (`stores::installer`) and a
serial queue (`stores::queue`), and `stores::status::updates` unified the "an
update is pending" signal across Steam (`StateFlags`) and the CLI stores (the
`needs-verification` tag). Nothing *applied* an update. And Proton/GE builds,
which M3's graphics work and M3.5's neural rendering both want to pin, could only
be dropped in by hand — where they land in Steam's own directories, which
PitaGames must not write to.

Two temptations had to be refused:

- a second "update" path (its own queue, its own worker, its own verdict), which
  would drift from the install path the first time a store changed its CLI;
- a silent, automatic Proton updater, which would change a runtime under a game
  the user is playing, and would have to trust whatever a release host served.

## Decision

- **A CLI-store update is an install of an existing id plus a verify.** The job
  carries a `JobKind` (`Install` default, `Update`); `installer::run` runs the
  backend's install, and for an update then the backend's own verify, against
  the same target, on the same serial queue and the same worker thread. The
  update's target is the game's own `install_dir` (falling back to the store's
  default only when the row does not know it), so an update can never move to a
  fresh directory and leave the real install behind.
- **The verify verdict is each backend's own**, not one exit-code rule:
  legendary's summary line, nile's error lines, gogdl's exit status. A verify
  that does not pass makes the whole job an error, exactly as a failed install
  does.
- **Steam updates stay Steam's.** A Steam row's update is the existing
  `steam://install/<appid>` hand-off plus the manifest poller; Steam writes its
  own files and there is no CLI to drive.
- **Proton/GE updates are opt-in and pinned.** A pin (host, project, tag, asset,
  SHA-256) ships as data; `platform::proton_update::apply` fetches it through the
  shared `mods::release` pipeline, refuses a byte stream whose digest is not the
  pin, and unpacks it into PitaGames' own `<data>/pitagames/proton`, which
  `platform::proton` discovery already scans (`ProtonSource::Managed`). Nothing
  is automatic: no startup check, no schedule, and `apply` reports what it did.
- **Store CLIs are the package manager's job.** `legendary`, `nile`,
  `lgogdownloader` and `gogdl` are never self-updated here.
- **The existing model is what both surfaces read.** The console Updates chip and
  the store-status list are both `stores::status::updates`; `stores update` (CLI)
  turns an entry into the same `InstallPlan`/`InstallJob` a fresh install uses,
  so neither can drift from the queue.

## Consequences

- Positive: one queue, one worker, one verdict for installs and updates; a
  cancelled install and a cancelled update stop at the same boundary, and the
  store's own files are never deleted.
- Positive: a Proton build cannot silently change, and a compromised or moved
  release is refused by the pin rather than installed.
- Positive: our own `proton/` directory keeps every write out of Steam's
  directories (and a managed build is honestly reported as not forceable in
  Steam — its `compat_tool_name` is `None`).
- Negative: a brand-new upstream GE-Proton release needs a new shipped pin (or a
  user override file) to be noticed; there is no "check for the latest".
- Negative: `mods::release::Fetcher` returns a whole body, so an update reports
  its result, not a download percentage. A streaming fetcher would change the
  trait.
- Follow-up: the GUI's Update button wiring (a CLI row that is installed and
  flagged offers **Update**, enqueuing `installer::update_plan(&game).job()`) is
  deliberately left to a later slice; nothing in `stores` changes for it.

## Alternatives considered

- **A separate update runner/queue.** Rejected — it is the drift this repository
  keeps paying to avoid, and the install queue already serializes for the
  reason (one login session, one `~/.config` cache).
- **Reinstall-only (no verify).** Rejected — Epic's `needs_verification` flag
  exists precisely because a download alone does not prove the files; the verify
  is what closes an update.
- **Fetching the newest GE-Proton from the GitHub API and trusting its digest.**
  Rejected by ADR-0024: the pin we ship is the trust anchor, and a host that
  contradicts its own published digest is not installed from.
- **Writing a managed build into Steam's `compatibilitytools.d`.** Rejected —
  that directory is Steam's, and a `-git` package must not fight a packaged
  Proton. Our own `proton/` dir is scanned by discovery instead.
- **Auto-updating Proton on startup.** Rejected — a runtime change under a
  running game is exactly the surprise the opt-in rule forbids.

## References

- `docs/m4-updates.md`; `docs/mods-primitives.md`, `docs/m2-store-status.md`,
  `docs/m2-store-installs.md`.
- plan §7 *Update flows*, §8 *Storage* (`proton/`), §12 *DLSSNR opt-in updates*.
- `docs/adr/0015`, `docs/adr/0019`, `docs/adr/0024`.
