# M1 — Proton discovery (`platform::proton`)

This is the runtime-discovery slice: before the launcher can compile a
`LaunchSpec`, pick a wrapper chain or write Steam's launch options, it has to
know which Proton builds the machine actually has. `crates/platform` answers
that question and nothing else — it never launches, installs or patches Proton.

Discovery is read-only. It lists directories, follows symlinks, reads one small
`version` file per candidate, and returns a report. Missing directories are
normal (a fresh system has several) and stay silent.

## Pieces

| Crate | Responsibility |
|---|---|
| `platform` (`pitagames-platform`) | `ProtonInstall` (one build), `ProtonKind`, `ProtonSource`, `ProtonRoot`, `DiscoveryReport`, `discover()` / `discover_in()` |
| `core` (`core::paths`) | `steam_roots()` and the new `data_home()` (`$XDG_DATA_HOME` without the `pitagames` suffix) |
| `cli` | `pitagames-cli proton` diagnostic (text + `--json`) |

`pitagames-platform` is framework-free and DB-unaware like every crate below
`app`. Roots are passed explicitly (`discover_in(&roots)`), so tests point at a
`tempfile` tree and never inspect the host.

## Where it looks

`default_roots()` builds this list, highest priority first:

| Root | Source | Notes |
|---|---|---|
| `/usr/share/steam/compatibilitytools.d` | `system` | distro packages; `proton-cachyos` installs here |
| `/usr/lib/steam/compatibilitytools.d` | `system` | older Steam packaging layout |
| `<steam>/compatibilitytools.d` | `user` | per Steam root; GE-Proton and umu downloads land here |
| `<steam>/steamapps/common` | `steam` | Valve's own builds; only entries named `Proton*` are considered |
| `$XDG_DATA_HOME/Steam/compatibilitytools.d` | `user` | where `umu-run PROTONPATH=GE-Proton` extracts builds |
| `$XDG_DATA_HOME/umu/compatibilitytools` | `umu` | UMU-Proton and umu's `*-Latest` codenames |

`UMU_FOLDERS_PATH` replaces the last two, matching umu-launcher's own
`UMU_FOLDERS_PATH/umu/compatibilitytools` and
`UMU_FOLDERS_PATH/Steam/compatibilitytools.d` layout.

`core::paths::steam_roots()` still supplies `PITAGAMES_STEAM_ROOT` and both
Linux Steam roots (`~/.local/share/Steam`, `~/.steam/steam`). Those two are
usually the same directory through a symlink, so roots and installs are
de-duplicated by canonical path after the priority sort.

## What counts as an installation

A directory is a Proton build when it contains a `proton` file (the launcher
script every release ships). Nothing else is required:

- a half-extracted `.part` directory or a locked download is skipped silently;
- a directory with a `version` file but no `proton` script is **not** a build;
- symlinked build directories are followed (users commonly link a Proton
  install into `compatibilitytools.d` instead of moving it);
- inside `steamapps/common`, only directories whose name starts with `Proton`
  are probed, so a game folder that happens to contain a `proton` file can
  never be offered as a runtime.

`ProtonInstall::from_dir` also accepts the `proton` script itself, so a file
picker that returns the executable still resolves to its build directory.

## Classification and ordering

`ProtonKind` is detected from the directory name plus the `version` file:

| Kind | Detection | `ProtonRef` |
|---|---|---|
| `CachyOs` | `cachy` in the name/version | `CachyOs` |
| `Ge` | `ge-proton`, `proton-ge`, `ge-latest` | `GeProton` |
| `Valve` | any other `proton` | `Auto` may pick it |
| `Experimental` | `experimental` | `Auto` may pick it |
| `Umu` | `umu` (UMU-Proton, UMU-Latest) | `Auto` may pick it |
| `Other` | neither name nor version mentions Proton | report only |

Selection order is:

1. kind rank — `cachyos > ge > valve > experimental > umu > other`;
2. newest build inside the kind, comparing numeric runs
   (`GE-Proton9-20` beats `GE-Proton9-9`, which beats `GE-Proton9-3`);
3. source rank — system > user > Steam > umu > explicit;
4. lexical path, so the order is deterministic.

Kind order is the plan's rule (proton-cachyos is recommended on CachyOS even
when a newer GE build exists); the version comparison only decides between
builds of the same kind.

The `version` file is cleaned up before display: Valve writes
`1700757745 proton_9.0` (numeric build id first), which becomes `proton_9.0`.
`ProtonInstall::distinct_version()` hides strings that repeat the directory name
(`GE-Proton9-20`, `Proton 9.0` / `proton_9.0`) and keeps values that add
information (`proton-cachyos 10.0-20251101`). Missing, empty or absurdly long
`version` files are treated as absent.

## Resolving a profile

`DiscoveryReport::resolve(&ProtonRef)` maps the profile field to a build:

| `ProtonRef` | Result |
|---|---|
| `Auto` | the first install, or `Err(NoneFound)` when nothing was found |
| `SteamDefault` | `Ok(None)` — leave Steam's compatibility tool alone |
| `CachyOs` | newest CachyOS build, or `Err(Unavailable { kind })` |
| `GeProton` | newest GE build, or `Err(Unavailable { kind })` |
| `Path { path }` | the matching discovered build, else a build validated from that path (`source: explicit`) |

`Ok(None)` is deliberately distinct from an error: a Steam game on
`SteamDefault` needs no `PROTONPATH`, while `Auto` with an empty report means
there is nothing to launch with and the UI must say so.

## CLI diagnostic

```bash
pitagames-cli proton                      # default roots on this machine
pitagames-cli proton --steam-root ~/.local/share/Steam
pitagames-cli proton --root /games/GE-Proton10-1 --json
```

```
Searched 4 location(s):
  /usr/share/steam/compatibilitytools.d                system
  /home/u/.local/share/Steam/compatibilitytools.d      user
  /home/u/.local/share/Steam/steamapps/common          steam
  /home/u/.local/share/umu/compatibilitytools          umu

Found 3 Proton installation(s), best first:
  proton-cachyos        proton-cachyos        proton-cachyos 10.0-20251101  /usr/share/steam/compatibilitytools.d/proton-cachyos
  GE-Proton9-20         GE-Proton             -                             /home/u/.local/share/Steam/compatibilitytools.d/GE-Proton9-20
  Proton 9.0            Steam Proton          -                             /home/u/.local/share/Steam/steamapps/common/Proton 9.0

Auto selects: proton-cachyos (/usr/share/steam/compatibilitytools.d/proton-cachyos)
```

`--root` scans a compatibility-tools-style directory; `--steam-root` expands to
`<dir>/compatibilitytools.d` plus `<dir>/steamapps/common`. Both replace the
default roots when present (same rule as `scan --steam-root`), which makes the
command usable against a fixture tree or a second Steam install. `--json` prints
`{ installs, auto, roots, warnings }` and is what scripted checks should parse.

## Failure policy

- A missing root is skipped silently.
- An unreadable root, or a directory entry that cannot be read, becomes a
  warning string (`<path>: <io error>`) and the rest of the scan continues.
- A candidate without the `proton` entrypoint is skipped silently — `.part`
  directories and locked downloads produce these constantly.
- Nothing is cached: discovery runs when the caller asks, and re-running after
  installing a build picks it up immediately.

## Verification

Pure crates (macOS host and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-platform -p pitagames-cli
CARGO_HTTP_MULTIPLEXING=false cargo clippy -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-platform` has 17 tests:

- discovery order across cachyos / GE / Experimental / UMU;
- `steamapps/common` name filter (a game with a `proton` file is ignored);
- newest-wins ordering (`9-20 > 9-9 > 9-3`) and version-file-over-directory-name;
- system builds winning a same-version tie;
- duplicate and symlinked roots / installs de-duplicated or followed;
- missing roots silent, a file used as a root warns;
- directories without the entrypoint skipped;
- `version` formats (Valve build-id prefix, plain GE, empty, over-long, absent);
- kind detection for every family, including `proton-cachyos-slr` and `GE-Latest`;
- `distinct_version()` hiding repeated build names;
- `Auto` / `SteamDefault` / `CachyOs` / `GeProton` / `Path` resolution, including
  a missing path and the `proton` entrypoint handed in directly;
- the root plan covering every documented location, and `UMU_FOLDERS_PATH`;
- JSON round-trip of a report (the CLI contract).

CLI tests cover argument parsing for `--root` / `--steam-root` / `--json` and
both the populated and empty report renderers.

Fixture smoke run on the macOS host (fake tree, no real Proton):

```bash
pitagames-cli proton --root <system> --root <user> \
  --steam-root <steam> --root <umu>/compatibilitytools
```

found all five fake builds, ordered `proton-cachyos`, `GE-Proton9-20`,
`GE-Proton9-3`, `Proton 9.0`, `UMU-Proton`, skipped the fake game folder in
`steamapps/common`, and selected `proton-cachyos` for `Auto`. `--json` emitted
the installs plus `auto`.

## Known limitations

- **No GPU awareness yet.** `Auto` follows the CachyOS-first rule; the
  per-GPU recommendation (RDNA 4 vs RDNA 1) arrives with `platform::gpu` and
  the capability matrix in M3.
- **No version gating.** Discovery reports what exists; whether a build is new
  enough for FSR 4 / OptiScaler / DLSSNR is the capability matrix's job.
- **umu's `*-Latest` codenames** are reported as `GE`/`UMU` builds with their
  `version` file; the codename itself (`GE-Latest`) is the directory name.
- **No launch integration yet.** Nothing consumes the report in the GUI and no
  `PROTONPATH` is set. The Steam launch-options writer now exists
  (`docs/m1-launch-options.md`) and can route a game through the shim; the GUI
  Proton picker and `platform::envc` still do not consume the report.
- **The CachyOS package path is assumed, not observed.**
  `/usr/share/steam/compatibilitytools.d` is where distro compatibility-tools
  packages install; it has not yet been checked on the two target machines
  (they are not reachable from the dev host). `pitagames-cli proton` is the
  one-line check.

## Deliberately not in this slice

- Downloading/updating GE-Proton or proton-cachyos (packaging stays with the
  distro/umu; Proton/GE updates are M4).
- Running `proton --version` or any other subprocess: the `version` file is
  cheaper and side-effect free.
- Per-game `CompatToolMapping` writes (`config.vdf`) — same guard rails as the
  launch-options writer (which now exists), and a post-v1 item.
- A GUI Proton picker: the writer now makes the shim line apply; wiring the
  picker to it is the next GUI task.
