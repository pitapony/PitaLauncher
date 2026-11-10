# M1 — anti-cheat detection and the injection gate (`platform::anticheat`)

PitaGames injects DLLs into game processes for OptiScaler, HelixSR and DLSSNR.
The one thing that can turn that from a feature into a ban is an anti-cheat
runtime that treats a modified game directory as a cheat, so the plan makes it a
**hard gate**: a game known to use Easy Anti-Cheat or BattlEye can only be
injected after the user explicitly confirms the risk, and never automatically.

This slice is detection plus the gate model. It injects nothing, installs
nothing and writes nothing — the graphics screen that will *enforce* the gate is
M3 ("injection-slot exclusivity model"), and until then `platform::envc` already
refuses every injected setting by name.

## Pieces

| Crate | Responsibility |
|---|---|
| `platform` (`pitagames-platform`) | `anticheat::detect` (the marker walk), `effective` (merge with curation) and `gate` (`GateDecision`) |
| `core` (`core::store`) | `AntiCheat::is_protected()` / `label()` — the rule the gate reads |
| `core` (`core::profile`) | `Profile::requests_injection()` — whether a profile actually asks for the slot |
| `app` (`crates/app/src/anticheat.rs`) | the framework-free detail-page summary (`AnticheatView`) |
| `cli` | `pitagames-cli anticheat` diagnostic |

`platform::anticheat` is framework-free and takes paths, like `platform::proton`
and `platform::compat`, so it is testable against `tempfile` trees.

## Two inputs, kept separate

- **Curation** — the compat DB's `[game] anticheat` field (`platform::compat`).
  It is right even when the game is not installed on this machine.
- **Detection** — `detect(install_dir)` walks an installed game directory for the
  marker files and directories EAC and BattlEye ship.

`effective(curated, scan)` merges them with one rule: **a concrete `Eac`/`BattlEye`
marker always wins**, otherwise the curated value is used. A stale compat entry
must never downgrade real evidence, and a scan that finds nothing must not
downgrade curation either, because anti-cheat frequently lives outside the game
folder (in the Proton prefix, behind a launcher, or as a separate service). The
merge returns an `AnticheatSource` — `Detected`, `Curated` or `Undetermined` — so
the UI can say where the answer came from.

## Markers

`MARKERS` is the table of names matched case-insensitively:

| Runtime | Directories | Files |
|---|---|---|
| Easy Anti-Cheat | `EasyAntiCheat/`, `EasyAntiCheat_EOS/` | `EasyAntiCheat.exe`, `EasyAntiCheat_x64.dll`, `EasyAntiCheat_x86.dll`, `EasyAntiCheat_EOS_x64.dll`, `EasyAntiCheat_EOS_x86.dll`, `start_protected_game.exe` |
| BattlEye | `BattlEye/` | `BEService.exe`, `BEService_x64.exe`, `BEClient.dll`, `BEClient_x64.dll` |

The walk is bounded so it stays cheap on a detail page even for a 200 GB game:

- **depth** — `MAX_DEPTH = 3` levels below the install directory. EAC/BattlEye
  sit at the game root (`BattlEye/`) or a level or two down (Unreal's
  `Binaries/Win64/EasyAntiCheat/`).
- **entries** — `MAX_ENTRIES = 50_000` directory entries. When the cap trips the
  scan is marked `truncated` so the answer is never mistaken for complete.
- entries are sorted before matching, so the finding order and the cap are
  deterministic regardless of the filesystem's directory order.

A matched marker directory is not descended into (it only holds the runtime). A
directory that does not exist yields `scanned = false`, which is distinct from
"walked and found nothing"; a per-directory read error is skipped rather than
failing the whole scan. Detection only **reads** — nothing here injects or
enables anything.

## The gate

```rust
gate(anticheat, requested, acknowledged) -> GateDecision
```

- `NotRequested` — the profile asks for no injection; the gate did not engage.
- `Allowed` — injection may run.
- `ConfirmationRequired { anticheat }` — injection is requested on a game known
  to use `Eac`/`BattlEye` and the user has not acknowledged it.

Two deliberate rules, both from the plan:

- **`Unknown` never hard-gates.** It would block every game whose anti-cheat we
  have simply not catalogued, and the plan reserves the hard gate for the two
  runtimes it names. The UI can still show an advisory.
- **`Auto` injection alone is not a request.** `Profile::requests_injection()`
  is true for an explicit tier (`EnvOnly`/`Manual`/`HelixSr`/`DlssNr`), an
  injected upscaler, frame generation, or neural rendering — not for the default
  "resolve from the GPU" placeholder, which resolves to nothing on its own.

`GateDecision::message()` is the copy the UI shows, naming the anti-cheat and the
risk. `GateDecision::allows()` answers "may injection run?" and
`confirmation()` returns the anti-cheat that forced a confirmation, so a caller
can thread the acknowledgement through.

## CLI diagnostic

```bash
pitagames-cli anticheat 1172470 --shipped ./compat                       # scan via the appmanifest
pitagames-cli anticheat 1172470 --dir /games/Steam/steamapps/common/Apex # scan an explicit directory
pitagames-cli anticheat 1172470 --shipped ./compat --json
```

```
Anti-cheat for steam:1172470
  install dir: /games/Steam/steamapps/common/Apex
  detected:    eac (1 marker)
      /games/Steam/steamapps/common/Apex/EasyAntiCheat  (eac)
  curated:     eac
  effective:   eac (detected)
  gate:        confirmation required before eac injection
      Easy Anti-Cheat can ban accounts for modified game files. Injecting here needs an explicit confirmation, and online play is never recommended.
```

A game with no markers and no compat entry reads:

```
  detected:    none (no markers found)
  curated:     unknown
  effective:   unknown
  gate:        injection allowed (no anti-cheat hard gate)
```

With no `--dir`, the install directory is resolved from the appmanifest scan
(`--steam-root`, else auto-detect); an uninstalled game still shows the curated
value with `install dir: (not installed here)` and `detected: unknown (not scanned)`.
`--shipped`/`--user` point at fixture compat directories, matching
`pitagames-cli compat`. `--json` prints `{ game, install_dir, scan, curated,
effective, gate }`.

## GUI

The game detail page shows an **Anti-cheat** info card (`Easy Anti-Cheat
(detected)`, `None (compat DB)`, `Not detected`, …) built by the framework-free
`crates/app/src/anticheat.rs`, and — when a concrete anti-cheat is present — an
amber warning box naming it and listing the marker files found. The scan runs
once per game open, off the render path.

## Verification

Pure crates (macOS host and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-core -p pitagames-platform -p pitagames-cli
CARGO_HTTP_MULTIPLEXING=false cargo clippy -p pitagames-core -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-platform` adds 13 `anticheat` tests: a clean directory finds nothing;
a missing directory is not scanned; EAC and BattlEye directories are found at the
root; markers are found in nested Unreal `Binaries/Win64`; matching is
case-insensitive; the entry cap truncates and the depth limit bounds the walk;
detection beats a stale compat entry; curation is trusted when nothing is
detected; an uncurated uninstalled game is undetermined; and the gate table
(not-requested, protected-needs-confirmation, acknowledged, unknown/absent
allowed).

`core` adds the `is_protected`/`label` rule and `Profile::requests_injection`.
`crates/app` adds 4 `anticheat` summary tests. `pitagames-cli` adds
argument-parsing and render tests.

The GUI is verified in `archlinux:latest`: `cargo build -p pitagames
--all-targets`, `cargo clippy -p pitagames --all-targets -- -D warnings` and
`cargo test -p pitagames` (60 tests, up from 56). An Xvfb smoke run against a
fixture Steam root (Apex Legends with an `EasyAntiCheat/` marker and
`PITAGAMES_COMPAT_DIR=./compat`) renders the detail page's **Anti-cheat** card
`Easy Anti-Cheat (detected)` and the amber injection warning naming the runtime.

## Known limitations

- **Not hardware-validated.** No real game directory has been scanned on the
  RX 9070 / RX 5700 XT yet, like the rest of M1. The marker table is the
  documented set for both runtimes; a miss is a data fix.
- **The gate is not enforced yet.** Nothing injects in M1 (`platform::envc`
  refuses graphics by name), so the gate has no bypass to close. The graphics
  screen (M3) is where confirmation is required before injection turns on, and
  `Profile::requests_injection` / `GateDecision` are the hooks it will read.
- **No acknowledgement is persisted.** Confirmation is modelled as an input to
  the gate, not yet a stored profile field; that lands with the graphics screen.
- **Directory-name heuristics only.** Detection looks for the runtimes' files; it
  cannot see kernel drivers or anti-cheat that lives entirely outside the game
  folder, which is exactly why curation is trusted when detection is silent.
- **`Unknown` is not a bug.** It is the honest default for a game with no
  curated entry and no markers; the UI must not imply "no anti-cheat".

## Deliberately not in this slice

- Enforcing the gate in `platform::envc` / the graphics compiler (M3).
- The graphics screen, the confirmation dialog and the injection-slot model (M3).
- Persisting an acknowledgement per profile.
- Anti-cheat *identity* beyond EAC/BattlEye (Vanguard, Denuvo, …).
