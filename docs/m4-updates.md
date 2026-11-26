# M4 — updates: store games on the install queue, Proton/GE opt-in

Status: `stores::installer`'s update path, `platform::proton_update` and the
`pitagames-cli stores update` / `pitagames-cli proton update` diagnostics are
implemented and unit-tested against fake CLIs and fixture archives on macOS.
**Not driven against a real store CLI or a real GE-Proton download** — no target
machine has been reachable, and the live HTTP `Fetcher` is the shared open item
from ticket #12. The GUI's Update button wiring is deliberately **not** in this
slice (see *Known limits*).

## Why this slice exists

M2 gave every CLI store an installer (`stores::installer`) and a serial queue
(`stores::queue`), and `stores::status::updates` unified the "an update is
pending" signal across Steam and the CLI stores. Nothing applied an update,
though, and Proton builds could only be dropped in by hand.

Two rules shape this slice:

- **An update is not a second path.** A CLI-store update is an install of an
  **existing** id plus a verify, so it rides the same queue, the same worker and
  the same verdict as a fresh install. Steam updates are unchanged (the
  `steam://install/<appid>` hand-off).
- **Proton/GE updates are ours, opt-in and pinned.** PitaGames never patches a
  Proton build and never writes into Steam's directories, but it can fetch a
  pinned GE-Proton release through the shared `mods::release` pipeline into its
  own `proton/` directory, where `platform::proton` discovery already looks.

**Store CLIs are the package manager's job.** `legendary`, `nile`,
`lgogdownloader` and `gogdl` are never self-updated here; that is the distro's
package.

## Store-game updates ride the install queue

| Item | Role |
|---|---|
| `queue::JobKind` | `Install` (default) / `Update`; `verifies()` is the one predicate |
| `InstallJob::as_update()` | the same job, marked as an update |
| `installer::update_plan(&Game)` | the plan for an installed game: the queue entry and the commands |
| `installer::VerifyRun` | the verify phase's verdict (per backend) |
| `InstallRun::verify` | the verify phase of an update run, `None` for an install |
| `installer::run` | dispatches on `job.kind`: install, or install **then** verify |

```text
  installer::update_plan(game) ──► InstallPlan { kind: Update, target: game.install_dir }
          │                                   │
          │                                   └─ .job() ──► InstallJob { kind: Update }
          ▼
  InstallQueue ──enqueue──► start_next ──► installer::run
                                                │  install phase ──► legendary/nile/gogdl
                                                └─ verify  phase ──► the same backend
```

- **The target is the game's own directory.** `update_plan` prefers the row's
  `install_dir` (written by the library sync or a previous install) and only
  falls back to the store's default when the row does not know it — re-deriving
  a path from the title would move the update and leave the real install alone
  on disk. `--dir` is the CLI's override for a row the cache does not hold.
- **One command sequence, one worker.** `installer::run` reads `job.kind`;
  `Install` runs the backend's install, `Update` runs the install and then the
  verify against the same target. There is no second runner and no second queue.
- **The verify verdict is each backend's own.** legendary's verify always exits
  `0`, so its verdict is the summary line (`epic::verify_report`); nile's repair
  exits `0` for a refusal too, so its verdict is its error lines
  (`amazon::verify_report`); gogdl's `repair` exits non-zero, so its exit status
  is the verdict. A verify that does not pass makes the whole job an error, the
  same way a failed install does.
- **A cancel stops at the same boundary.** Cancelling during the install leaves
  the run cancelled with `verify: None`; cancelling during the verify reports a
  cancelled `VerifyRun`. Either way the files the CLI wrote are the store's to
  repair — nothing is deleted.

### The chip, the status list and the queue

`stores::status::updates` is the one model the console **Updates** chip
(`app::filter`, via `needs_attention`) and the **store-status list** already
read. This slice makes that model *applicable*:

```bash
pitagames-cli stores update --store epic Gizmo --title "Gizmo" --dir <install dir>
pitagames-cli stores update --store gog 1207660623 --dry-run
```

`stores update` builds the same `InstallPlan`/`InstallJob` a fresh install does
(with `JobKind::Update`), puts it on the same `InstallQueue`, and runs the same
`installer::run` the GUI's `pump_installs` worker runs — so the queue, the worker
and the verdict cannot drift from an install. `--dry-run` prints the two planned
commands (install, verify) with the shared `render_shell_command` and runs
nothing.

### Steam updates stay Steam's

A Steam row's update is still `steam://install/<appid>` plus the manifest
poller (`library::spawn_install` + `downloads::track`) — unchanged. Steam writes
its own files; there is no CLI to drive and no verify to run.

## Proton/GE updates (`platform::proton_update`)

| Item | Role |
|---|---|
| `proton_update::ProtonPin` | one pinned build: `name`, `kind`, host, project, tag, asset, SHA-256 |
| `proton_update::Registry` | the pins: `shipped()` (embedded) + a user file, merged by name |
| `proton_update::SHIPPED` | `crates/platform/data/proton-updates.toml`, `include_str!`d |
| `proton_update::default_dir()` | `<XDG_DATA_HOME>/pitagames/proton` |
| `proton_update::default_user_path()` | `<XDG_CONFIG_HOME>/pitagames/proton/updates.toml` |
| `proton_update::status` / `available` | each pin's installed state; the opt-in candidates |
| `proton_update::apply(pin, dir, fetcher)` | fetch → verify → unpack, then confirm it is a build |
| `proton_update::UpdateReport` | what landed: asset, verified SHA-256, path, file count |
| `proton_update::ProtonUpdateError` | `Release` / `NoDataDir` / `NotAProtonBuild` |

```text
  Registry (shipped + user) ──► ProtonPin
          │                        │
          ├─ status(dir)           └─ .pinned() ──► PinnedRelease (mods::release)
          └─ available(dir)                            │  fetch
                 │                                     │  SHA-256 vs the pin
                 └──── user opts in ──► apply ──────────┤  unpack (links allowed, contained)
                                                       ▼
                              <data>/pitagames/proton/GE-Proton11-14-x86_64/proton
                                                       │
                              platform::proton discovery (ProtonSource::Managed) picks it up
```

- **Pinned, never the API's digest.** A pin fixes the host, tag, asset and
  SHA-256. `mods::release` refuses a byte stream whose digest is not the pin, and
  also refuses a host whose own published digest contradicts the bytes. A moved
  tag or a swapped artifact cannot silently become a different build. `name` is
  the archive's top-level directory (`GE-Proton11-14-x86_64`), which is also the
  directory under `proton/`.
- **`.tar.gz` and contained symlinks.** GE-Proton ships a gzip tar with relative
  symlinks (its built-in-DLL links, e.g. `files/bin/msidb -> wine`), so
  `mods::release` gained `ArchiveKind::TarGz` and a `LinkPolicy`. The update
  path uses `LinkPolicy::AllowContained`, which creates a symlink only when its
  target is **relative** and stays inside the destination — the `tar` crate does
  not validate a symlink's target, so `mods::release` does.
- **Into our own directory, not Steam's.** The build lands in
  `<data>/pitagames/proton`, which `platform::proton::default_roots` scans as
  `ProtonSource::Managed`. A managed build is discovered and selectable like any
  other (its `kind` is `Ge`, so `ProtonRef::GeProton` finds it); it is **not**
  given a `compat_tool_name`, because it lives outside Steam's search paths and
  forcing a name Steam would ignore is a lie. A build the user dropped into
  `compatibilitytools.d` still wins a same-version tie.
- **Never silent.** Nothing runs on startup or on a schedule. A caller lists the
  pins and their state, shows the tag and the digest, and calls `apply` only when
  the user opts in; `apply` returns what it did, and a wrong digest or an archive
  that is not a Proton build is an error, never a reported success.

```bash
pitagames-cli proton update                 # list the pins and their state
pitagames-cli proton update --json
pitagames-cli proton update --apply GE-Proton11-14-x86_64 --fixture <dir>
```

`proton update --apply` takes a `--fixture <dir>` (the `mods release` shape:
`release.json` plus the pinned asset's bytes) because the live HTTP `Fetcher` is
the shared open item from ticket #12 — `mods::release` ships the trait, the
`MapFetcher` fixture and the CLI diagnostic, and the production client plugs in
once. A `--user <file>` registry lets a user move a build to a newer release
without waiting for a PitaGames update.

## Verification

Pure crates (macOS host and CI):

```bash
cargo test -p pitagames-stores -p pitagames-mods -p pitagames-platform -p pitagames-cli
cargo clippy -p pitagames-stores -p pitagames-mods -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

- **`stores::installer` (7 update tests, stores 272).** An update plan reuses the
  directory the game is installed in (and the queue entry is an update); it falls
  back to the store's directory when the row does not know one; it refuses a
  store with no installer; an update installs **and then** verifies (both fake
  markers written, `run.verify.verified`); a plain install never runs a verify; a
  verify whose summary says files are corrupt is refused (`epic update verify
  failed`); a GOG update judges the verify by gogdl's exit status; an update
  cancelled before it starts has `verify: None`. `queue` adds 2 (`JobKind`
  defaults, `as_update` keeps the target).
- **`mods::release` (5 new, mods 57).** A gzip tar is detected by magic and
  round-trips through fetch → verify → unpack; the four formats (zip, tar.zst,
  tar.gz, tar) all unpack; a symlink is refused by default; a contained relative
  symlink is created under `AllowContained`; an absolute or climbing symlink is
  refused even then.
- **`platform::proton_update` (7) + `platform::proton` (2).** The shipped
  registry parses and pins a real GE-Proton build (64-hex digest); a user pin
  replaces a shipped one by name; `status`/`available` follow the directory;
  `apply` fetches, verifies and unpacks into the managed dir (and
  `ProtonInstall::from_dir` accepts the result); a digest mismatch is refused
  before anything is written; an archive with no `proton` entrypoint is refused;
  a user registry file overrides the shipped pins; a missing user registry is not
  an error. Discovery finds a build in `pitagames/proton` as
  `ProtonSource::Managed` with no `compat_tool_name`, and the default root plan
  includes it.
- **`pitagames-cli` (2 new + extended guards, cli 66).** `stores update` runs an
  install then a verify through the queue and the dry-run starts nothing; the
  parser rejects a stray positional, a second `--store` and `--db` on `update`;
  `proton update` lists (plain and JSON) and applies a pinned fixture, refuses a
  mismatched pin, and routes through the `proton` dispatcher.

Manually (macOS), with a fake `legendary` on `PATH`:
`pitagames-cli stores update --store epic Gizmo --title Gizmo --dir <tmp>` ran
the fake's `install` then `verify` and printed `Updated Gizmo (epic Gizmo)` with
`verify legendary: files match`; `--dry-run` printed the two commands.
`pitagames-cli proton update` listed the shipped `GE-Proton11-14-x86_64` pin as
`not installed`; `--apply … --fixture <dir>` (a hand-built gzip tar with a
contained symlink) unpacked it and `pitagames-cli proton --root <dir>` listed it
as a GE-Proton build.

## Known limits

- **Not driven against a real store CLI or a real GE-Proton download.** The
  store argv/verdicts come from the shipped fixtures; the archive shape and the
  shipped digest come from GE-Proton's own release API (the x86_64 asset's
  published SHA-256), not a completed download.
- **The GUI does not offer the update yet.** `platform::proton_update`'s `apply`,
  `stores::queue`'s `JobKind::Update` and the CLI paths all exist, but the
  app-side wiring is deliberately out of this slice (the app build is serialized
  and cannot run concurrently). What remains is small and named: a CLI row that
  is installed **and** carries `stores::status::UPDATE_TAG` should offer
  **Update**, and its action should enqueue `installer::update_plan(&game).job()`
  instead of `installer::plan(id, title).job()` — the same `Installs` board and
  `pump_installs` worker then run it (`InstallRun` already carries the verify).
  Nothing in `stores` changes to wire it.
- **The live HTTP fetch is the shared open item.** `proton update --apply` reads
  a fixture, exactly like `mods release`; a real `Fetcher` is ticket #12's
  production plug-in, not this slice's.
- **No download progress.** `mods::release::Fetcher` returns the whole body at
  once, so `apply` reports the result, not a percent. A streaming fetcher would
  change the trait.
- **A managed build is not forceable in Steam.** `compat_tool_name` is `None`
  for `ProtonSource::Managed` (it lives outside Steam's search paths), so the
  Runtime panel's "Force in Steam" does not apply to it — it is chosen through
  the shim profile / umu `PROTONPATH` instead.
- **A new upstream release needs a new pin.** The registry ships pins; noticing
  a brand-new GE-Proton release requires a PitaGames update or a user override
  file. The CLI has no "check for the latest" command, which is the same
  opt-in/pinning stance ADR-0024 takes for the mod backends.
- **The `aarch64` asset is not pinned.** The target machines are x86_64; a user
  on another arch would add their own pin.
