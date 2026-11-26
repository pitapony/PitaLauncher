# Tier B OptiScaler (`mods::tierb`) — managed deploy and uninstall

Status: implemented and verified over a `tempfile` game tree and a fixture release
archive on macOS. The Graphics screen that offers it is a later ticket (#24), and
the shipped tool-pin registry is M3 — this slice resolves the exe dir, picks a DLL
name, fetches/verifies, patches, writes the manifest and returns the env. No real
OptiScaler release has been fetched and no real game has been deployed into yet.

## Why this slice exists

Tier A is Proton's own, env-only OptiScaler hook. Tier B is the fallback for a
game running under Valve Proton / GE / an older build, or when the user wants
OptiScaler's in-game menu: the launcher drops OptiScaler's files into the game's
own directory, backs up whatever it displaces, and hands the launch a
`WINEDLLOVERRIDES` value (ADR-0023). It is the first real consumer of the three
shared primitives (#12) and of the one mod ledger (#11):

- [`mods::release`](mods-primitives.md) fetches the pinned asset and verifies its
  SHA-256 against our pin (ADR-0024);
- [`mods::ini`](mods-primitives.md) patches the shipped `OptiScaler.ini`
  line-preservingly;
- [`mods::manifest`](mods-manifest.md) records every file, backs up every
  original to `.gl-backup/`, and restores the tree byte-for-byte.

## Pieces

| Item | Role |
|---|---|
| `tierb::resolve_exe_dir(target)` | the directory the files belong in (Unreal `*/Binaries/Win64` aware) |
| `tierb::DllName` | the candidate list `dxgi → d3d12 → dbghelp → winmm → version` |
| `tierb::choose_dll(exe_dir)` | the first candidate the game does not already own |
| `tierb::override_env(dll)` | `WINEDLLOVERRIDES="<name>=n,b"` as a `Vec<(String, String)>` |
| `tierb::deploy(target, pin, fetcher, staging, settings)` | fetch → verify → patch → copy → manifest → env |
| `tierb::uninstall(target)` | restore the tree from the manifest |
| `tierb::Deployment` | exe dir, chosen DLL, env vector, fetched release, manifest |
| `tierb::TierBError` | every failure, named |
| `pitagames-cli mods tierb` | the diagnostic (deploy or `--uninstall`) |

## Resolving the exe directory

`resolve_exe_dir` takes the game's **executable** (the usual input) or its
install directory:

- an executable path resolves to the directory it sits in;
- an Unreal shipping exe lives under `*/Binaries/Win64/`, and that `Win64`
  directory is where injection belongs — passing either the exe or the project
  directory that holds `Binaries/Win64` reaches it;
- any other directory is used as it is.

A path that is neither a file nor a directory is a named `TargetMissing`.

## Choosing the DLL name

The candidate list is walked in order and the first name whose file **does not
already exist** in the game directory wins. That is how a name the game (or
another mod) owns is honoured: this module never clobbers such a file, so a game
that ships its own `dxgi.dll` gets OptiScaler as `d3d12.dll` (or later). When
every candidate exists the deploy is refused (`NoFreeDll`) rather than forcing a
collision. The "one injection slot" is enforced by the manifest as well: a
second deploy is refused until the first is uninstalled.

## Deploy

```text
deploy:  resolve exe dir ─► choose free DLL ─► fetch + SHA-256 verify ─►
         unpack to staging ─► patch OptiScaler.ini ─► copy DLL (renamed) +
         OptiScaler.ini + the OptiScaler/ folder ─► write the manifest ─►
         env = [("WINEDLLOVERRIDES", "<name>=n,b")]
```

Only the release's known files are copied — the DLL renamed to the chosen name,
the ini, and everything inside the `OptiScaler/` folder; a root `README.md` or
`LICENSE` is not dropped into the game. The ini is patched **in staging** before
it is copied, so the game directory only ever sees the already-patched file; a
settings list with no ini to patch is a broken release and is refused rather than
silently dropped. A pin whose SHA-256 does not match is refused before the tree
is touched (the manifest is not written and the game directory is unchanged).

The `WINEDLLOVERRIDES` env is returned as a `Vec<(String, String)>` — the same
shape `core::launch::LaunchSpec::env` uses — never as a shell string (acceptance
criterion, ADR-0023).

## Uninstall

`uninstall` resolves the same exe directory and calls `ModManifest::restore`. The
backups are verified first (a tampered backup is reported, never restored over),
the files the mod created are deleted, the originals are moved back, the created
directories are pruned, and the report says whether the tree hash returned to the
pre-install value.

## CLI diagnostic

```bash
pitagames-cli mods tierb <target> --fixture <dir> [--staging <dir>] [--set Section.Key=Value]... [--json]
pitagames-cli mods tierb <target> --uninstall [--json]
```

`<target>` is the game executable or install directory. `--fixture` is the same
release fixture `mods release` uses (`fixture.json` + `release.json` + the
archive), so the whole fetch → verify → unpack pipeline runs offline. `--set`
patches an `OptiScaler.ini` value. The printed result names the exe dir, the
chosen DLL and the env vector; the `--json` payload carries `env` as an array of
`[key, value]` pairs. `--uninstall` restores from the manifest and exits non-zero
when the tree hash does not return.

## Verification

Pure crates (macOS host and CI):

```bash
cargo test -p pitagames-mods -p pitagames-cli
cargo clippy -p pitagames-mods -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-mods` ships 65 tests (52 before; 13 new in `tierb`):

- **DLL choice (2)** — `choose_dll` walks the candidate list in order and
  `override_env` names the chosen DLL.
- **exe dir (1)** — an Unreal shipping exe, its project directory and the `Win64`
  directory all resolve into `Win64`; a plain exe resolves to its own directory;
  a missing path is refused.
- **deploy (7)** — the DLL, ini and release folder land and the manifest is
  written (a root `README.md` is not copied); the env is a `[(WINEDLLOVERRIDES,
  dxgi=n,b)]` vector with no shell characters; an occupied `dxgi.dll`/`d3d12.dll`
  is skipped and left untouched; the ini is patched line-preservingly (a comment
  survives); every name taken is `NoFreeDll` and writes nothing; a mismatched pin
  is refused without touching the tree; a second deploy is `AlreadyInstalled`.
- **uninstall (3)** — install → uninstall returns the pre-install tree hash and
  prunes the release folder; a game-owned `dxgi.dll` is left untouched across the
  deploy/uninstall cycle; uninstall with no manifest is a named error.

`pitagames-cli` adds one test: deploy over a fixture game tree (writing the
manifest and the override and patching the ini), the occupied-name skip, the
uninstall back to the original hash, and the wrong-pin refusal.

## Known limits

- **Not driven against a real OptiScaler release or a real game.** The archive
  layout (`OptiScaler.dll` / `OptiScaler.ini` / `OptiScaler/` at the root) is the
  shape upstream publishes; the fixture is hand-built. The shipped pin registry
  and the first real fetch are M3 (#24).
- **The pin is a caller parameter, not shipped data.** `deploy` verifies whatever
  `PinnedRelease` it is given; the tool-pin registry (a shipped `Release` pin
  with a user-override dir) is M3/M3.5 and is deliberately not in this slice.
- **No graphics-screen wiring.** `crates/app` is untouched; the deploy is offered
  through the CLI only. Reading the resulting env into `platform::envc`, the
  "Apply & verify" flow and the pre-flight conflict scan are #24.
- **The candidate list lives here, not in `core`.** `core::InjectionDll` and
  `platform::compat::PreferredDll` carry the same order; `crates/mods` stays
  framework-free and dependency-free of `core`, so the five-element list is
  stated once here too.
- **The exe dir is the manifest root** (the Unreal `Win64` directory for an
  Unreal game), so `.gl-backup/` and the ledger sit beside the injected files.

## Deliberately not in this slice

- The tool-pin registry, the per-game `compat/<game>.toml` overlay and the
  Graphics screen (`View::Graphics`, "Apply & verify", one-click revert) — #24.
- `mods::conflict` (the pre-flight scan that names existing mod files and their
  owners) and `mods::verify` (launch-log markers) — later tickets.
- Running a real upstream installer in a PTY; Tier B copies the release it
  fetched, it does not run a script.
