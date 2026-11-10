# M1 — the compatibility database (`platform::compat`)

The compat DB is the curated, shipped knowledge base the graphics layer reads:
what a game's upscaling actually supports, which option to recommend for each
GPU architecture, the preferred injection DLL, known runtime issues and whether
DLSSNR neural rendering is available. This slice is its **loader, merge and
validation**, plus the first curated entries. It compiles nothing and launches
nothing — the capability matrix and `platform::gpu` that consume it are M3.

## Pieces

| Crate | Responsibility |
|---|---|
| `platform` (`pitagames-platform`) | `CompatDb`, `CompatEntry`, the typed rules (`UpscalingRules`, `RuntimeRules`, `NeuralRules`), `Arch`, `Version`, the `*Id` enums and `load_from()` / `load()` |
| `core` (`core::paths`) | `compat_dir()` — the user-override directory |
| `core` (`core::store`) | `Store`/`GameId` gain `Ord` so a `GameId` can key the index |
| `cli` | `pitagames-cli compat` diagnostic (list, one entry, `--json`) |
| `compat/` | the shipped `*.toml` entries (30 curated Steam titles) |

`pitagames-platform` stays framework-free and DB-unaware. Directories are passed
in explicitly (`load_from(&shipped, &user)`), so tests point at a `tempfile` tree
and never read the host.

## Where the files live

| Layer | Directory | Notes |
|---|---|---|
| shipped (system) | `/usr/share/pitagames/compat` | installed by the package |
| shipped (synced) | `$XDG_DATA_HOME/pitagames/compat` | optional weekly-sync copy (post-v1) |
| user overrides | `$XDG_CONFIG_HOME/pitagames/compat` | `core::paths::compat_dir()` |

`PITAGAMES_COMPAT_DIR` replaces the shipped directories with one path, which is
how a development checkout (`compat/`) is used without installing anything:

```bash
PITAGAMES_COMPAT_DIR=./compat pitagames-cli compat
```

Missing directories are normal and stay silent. A path that exists but is not a
directory, an unreadable directory, an unreadable file and a file that does not
parse all become **warnings**, never failures: a broken user override must not
keep the launcher from starting.

## File shape

One entry per file, TOML, with a top-level `[game]` table and three optional
groups. The game identity always comes from the *content* (`store` + `store_id`)
and never from the file name, so a user override names the game with
`store`/`store_id` and then sets only the fields it wants to change — every
other field is inherited from the shipped entry.

```toml
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
engine = "REDengine"                 # optional
anticheat = "none"                    # none | eac | battleye | unknown
verified = ["rdna4"]                  # architectures this entry ships for

[upscaling]
supported = ["dlss", "fsr3", "xess"]  # the game's own upscalers
recommended = { rdna4 = "dlss_to_fsr4", rdna1 = "hlx_dlss", rtx = "dlss_native", arc = "dlss_to_xess" }
preferred_dll = "dbghelp.dll"         # Tier B OptiScaler DLL preference
helixsr = { modes = ["scaler", "fsr31_dropin"], preferred = "scaler" }
notes = "FSR 4 needs PROTON_FSR4_UPGRADE >= 4.0.3 for MLFG"

[runtime]
wrappers = ["gamemoderun"]            # optional wrapper suggestions
env = { }                             # optional pinned environment
known_issues = ["HDR + gamescope needs --hdr-enabled"]

[neural_rendering]                    # present => DLSSNR is available
backend = "auto"                      # auto | rdna4_upstream | rdna3_fork
route = "optiscaler"                  # optiscaler only in v1
min_mesa = "26.2"
dlssnr_min_version = "0.0.8"
model_scale_default = { rdna4 = 100, rdna3 = 50 }
requires_64bit = true
notes = "End = NR Control, Insert = OptiScaler"
```

The typed wire shape is strict: **unknown keys are rejected** (a typo becomes a
warning, not a silently ignored setting) and enum values are parsed, so an
unknown store, architecture, upscaler id, DLL name or version is caught at load
time.

### Recommended upscalers

`recommended` maps an `Arch` to an `UpscalerId`; `UpscalerId::to_upscaler()`
turns it into the launch-profile `Upscaler` for M3. `dlss_native` is the RTX
case (leave the game's own DLSS on) and maps to `Upscaler::Native`. The id set is
the plain-string mirror of `core::profile::Upscaler` plus that one extra.

### Versions

`min_mesa` and `dlssnr_min_version` parse to a `Version { major, minor, patch }`
(`26.2`, `26.2.3`, `0.0.8`; a missing patch is `0`). Ordering follows the numeric
fields, ready for the M3 "Mesa ≥ X" gate. A malformed version is a parse error.

## Merge

Shipped entries are the baseline. A user file with the same `store`/`store_id`
overrides it **field by field**: only the keys the user sets replace the shipped
ones; a game the shipped DB does not know is added. A duplicate *within* one
layer (two shipped files for the same game, or two user files) is a warning and
the later file wins, so the curated set stays duplicate-free (`compat/` loading
is asserted warning-free in a test).

Merging happens on the wire types before validation, so the merged result is
validated once with the source files recorded for the diagnostic.

## Validation

A merged entry is rejected, with the contributing files named, when:

- `[game] store`, `store_id` or `title` is missing or blank;
- a neural `model_scale_default` value is outside `25..=100`;
- `neural_rendering.route` is anything but `optiscaler` (the ReShade feeder
  routes are post-v1 and the profile validation refuses them by name);
- `helixsr.preferred` is not one of the listed `helixsr.modes`;
- an `[runtime] env` key is not a valid environment variable name.

Parsing rejects unknown keys and unknown enum values, as above.

## CLI diagnostic

```bash
pitagames-cli compat                       # list every entry
pitagames-cli compat steam 1091500         # one entry in detail
pitagames-cli compat --shipped ./compat    # a checkout instead of the package
pitagames-cli compat --shipped ./compat --user ./overrides --json
```

```
Compat DB: 30 entries from 30 file(s)
  steam:1091500    Cyberpunk 2077                           rdna4       dlss,fsr3,xess    dlss_to_fsr4
  steam:870780     Control Ultimate Edition                 rdna4       dlss,fsr2         dlss_to_fsr4
  ...

Cyberpunk 2077  [steam:1091500]
Anti-cheat:   none
Engine:       REDengine
Verified:     RDNA 4 (RX 9000)

Upscaling:
  supported:     DLSS, FSR 3, XeSS
  recommended:
    RDNA 4 (RX 9000) -> dlss_to_fsr4
    ...
Neural rendering:
  backend:       auto (from the GPU)
  route:         OptiScaler
  ...
```

Warnings go to stderr; `--json` prints `{ entries | entry, warnings }`.

## Verification

Pure crates (macOS host and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-platform -p pitagames-cli
CARGO_HTTP_MULTIPLEXING=false cargo clippy -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-platform` adds 17 `compat` tests:

- a full entry parses, is `Eq`-comparable and round-trips through JSON;
- recommendation → profile-upscaler mapping (including `dlss_native`);
- a minimal `[game]`-only entry is valid;
- required fields are reported;
- unknown keys/values, bad versions and out-of-range model scales are refused;
- the post-v1 ReShade routes are refused by name;
- invalid `[runtime] env` keys are refused;
- `helixsr.preferred` must be a listed mode;
- version parsing is strict and ordered;
- a user override merges field-by-field over shipped (untouched fields survive);
- a user can add a game the shipped DB does not know;
- duplicates within a layer warn and the later file wins;
- broken files (bad TOML, no `[game]`, missing `title`) become warnings;
- missing directories are silent and a file in their place warns;
- **the repository's real `compat/` loads with zero warnings, ≥ 30 entries and
  every entry verified for RDNA 4** — the guard that keeps a bad curation commit
  out, and the milestone's "RDNA 4 verified" target;
- the `PITAGAMES_COMPAT_DIR` override and the system default directory.

`pitagames-cli` adds argument-parsing and both renderers (detail + list/empty)
tests.

## Known limitations

- **Not hardware-validated.** `verified = ["rdna4"]` records the entry's target
  and curation, per the plan's "RDNA 4 verified" milestone; the RX 9070 run that
  would confirm the recommendations in-game is still blocked on machine access,
  like the rest of M1. Non-RDNA-4 architectures ship as `experimental`/unverified
  and the UI must say "not tested by us".
- **No GPU awareness / no consumer yet.** Nothing selects an `Arch` from a real
  card and no GUI surface reads the DB; `platform::gpu`, the capability matrix
  and the graphics screen are M3. `pitagames-cli compat` is the only consumer.
- **Seed data is best-effort.** The 30 entries reflect the titles' published
  upscaling support; a wrong `supported` list is a data fix, not a code change,
  and the loader's strictness makes those fixes loud.
- **No weekly sync.** The synced copy under the data directory and the GitHub
  sync (offline-safe, user-toggleable) are post-v1; only the on-disk layout is
  reserved here.
- **No SQLite copy.** The database is loaded from files each time; nothing is
  cached or merged into `library.db`.

## Deliberately not in this slice

- `platform::gpu` / the capability matrix that turns `Arch` into availability.
- Reading the DB into a launch profile or the `platform::envc` compiler.
- The graphics screen, "why disabled" copy and the wizard (M3).
- Anti-cheat *detection* (EAC/BattlEye directories): the DB records the curated
  `anticheat` value; scanning for the runtimes is the M1 follow-up slice.
