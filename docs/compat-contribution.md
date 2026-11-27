# Contributing to the compatibility database

The compatibility database (**the compat DB**) is the curated, shipped knowledge
base the graphics layer reads: what a game's upscaling actually supports, which
option to recommend for each GPU architecture, the preferred injection DLL,
known runtime issues, whether DLSSNR neural rendering is available and whether
Epic has enabled **EAC Linux v2** for the title. It is **read-only data** — adding
an entry changes recommendations and gating, never `crates/` code
(ADR-0025).

This guide is written so that **adding an entry is copy-paste**: copy one file
from `compat/`, change the identity and the fields you know, run the checker, open
a PR. If you only remember one thing, remember the checker:

```bash
pitagames-cli compat --shipped ./compat
```

A wrong or stale entry is a data bug, not a code bug, and the loader is strict
enough to make it loud: unknown keys and unknown values are warnings, so a typo
cannot hide as a silently ignored setting.

> **Not tested by us.** Only the **RX 9070** (RDNA 4) and **RX 5700 XT** (RDNA 1)
> are validated in-house. Every other architecture ships `experimental` and the UI
> must say "not tested by us" (ADR-0004). `verified` records what the entry was
> curated for; it is a claim, not a hardware test.

## Where the files live

One entry per file, one game per file, named `compat/<store>-<store_id>.toml`
(for example `compat/steam-1091500.toml`). **The file name is a convention only —
the game identity always comes from the `[game]` contents** (`store` +
`store_id`), never the name. That is what lets a user override inherit from a
shipped entry.

| Layer | Directory | Who writes it |
|---|---|---|
| shipped (system) | `/usr/share/pitagames/compat` | the package |
| shipped (synced) | `$XDG_DATA_HOME/pitagames/compat` | reserved for a post-v1 weekly sync |
| user overrides | `$XDG_CONFIG_HOME/pitagames/compat` | you, on your own machine |
| this checkout | `./compat` via `PITAGAMES_COMPAT_DIR=./compat` | a contributor |

`PITAGAMES_COMPAT_DIR` **replaces** the shipped directories with one path, which
is how a checkout is used without installing anything. `pitagames-cli compat
--shipped <dir> --user <dir>` overrides per invocation instead.

To contribute to the **shipped** set, add your file to `compat/` in the
repository. To fix your own machine without a PR, drop the same file into
`$XDG_CONFIG_HOME/pitagames/compat`; it merges over the shipped entry field by
field.

## The copy-paste template

Start from a shipped entry and delete what you do not know. This is the smallest
valid file — the three required keys and nothing else:

```toml
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
```

Everything below is optional and additive. This is a fully populated entry; the
comments name the allowed values:

```toml
# One line of prose is fine here: a file may open with comments.
[game]
store = "steam"                 # store, epic, amazon, gog, local, …
store_id = "1091500"            # the store's own id; a string, quoted
title = "Cyberpunk 2077"        # the display title the UI shows
engine = "REDengine"            # optional free text, shown in About
anticheat = "none"              # none | eac | battleye | unknown
eac_v2 = false                  # true when Epic enabled EAC Linux v2 (ADR-0031)
verified = ["rdna4"]            # architectures this entry ships for; [] = unverified

[upscaling]
supported = ["dlss", "fsr3", "xess"]   # what the game's own settings offer
recommended = { rdna4 = "dlss_to_fsr4", rdna1 = "hlx_dlss", rtx = "dlss_native", arc = "dlss_to_xess" }
preferred_dll = "dbghelp.dll"          # manual OptiScaler DLL preference
helixsr = { modes = ["scaler", "fsr31_dropin"], preferred = "scaler" }
notes = "FSR 4 needs PROTON_FSR4_UPGRADE >= 4.0.3 for MLFG"

[runtime]
wrappers = ["gamemoderun"]             # suggestions only
env = { }                              # pinned environment (usually leave empty)
known_issues = ["HDR + gamescope needs --hdr-enabled"]

[neural_rendering]                     # PRESENT means DLSSNR is available
backend = "auto"                       # auto | rdna4_upstream | rdna3_fork
route = "optiscaler"                   # optiscaler is the only v1 route
min_mesa = "26.2"                      # minimum Mesa
dlssnr_min_version = "0.0.8"           # minimum DLSSNR package version
model_scale_default = { rdna4 = 100, rdna3 = 50 }   # 25..=100
requires_64bit = true
notes = "End = NR Control, Insert = OptiScaler"

[ingame]                               # optional: override the hint note only
notes = "Back up your save before this one"
```

After editing, run the checker and read stderr:

```bash
pitagames-cli compat --shipped ./compat                 # list every entry
pitagames-cli compat --shipped ./compat steam 1091500   # one entry, in detail
pitagames-cli compat --shipped ./compat --json          # machine-readable
```

Warnings go to **stderr** and never stop the loader: a bad file is skipped, every
other entry still loads. A clean run prints no warnings. The repository's own
`compat/` is asserted warning-free in a test, so a curation mistake fails CI.

## Field reference

### `[game]` — required

| Key | Type | Notes |
|---|---|---|
| `store` | enum | The store this id belongs to (`steam`, `epic`, `amazon`, `gog`, `local`, …). |
| `store_id` | string | The store's own id, quoted. Steam appids are strings here. |
| `title` | string | The display title. Must not be blank. |

### `[game]` — optional

| Key | Type | Notes |
|---|---|---|
| `engine` | string | Free text, shown in the About block. |
| `anticheat` | enum | One of `none`, `eac`, `battleye`, `unknown`. A real EAC/BattlEye marker on disk always wins over this curated value. |
| `eac_v2` | bool | `true` when the publisher has enabled **Easy Anti-Cheat Linux v2** for the title — a fact a launcher cannot read from disk, so it is curated here (default `false`). See [`docs/eac-v2.md`](eac-v2.md). |
| `verified` | array | The architectures (`rdna4`, `rdna3`, `rdna2`, `rdna1`, `rtx`, `arc`) this entry is offered for without a "not tested by us" note. Empty means unverified. |

### `[upscaling]`

| Key | Type | Notes |
|---|---|---|
| `supported` | array | The APIs the game's own settings can select: `dlss`, `fsr2`, `fsr3`, `fsr31`, `fsr4`, `xess`. Use `[]` for a game with none (a protected online game, for example). |
| `recommended` | table | Maps an architecture to the upscaler to recommend for it. Every key is optional. |
| `preferred_dll` | string | The DLL a **manual** OptiScaler deploy prefers: `dxgi.dll`, `d3d12.dll`, `dbghelp.dll`, `winmm.dll` or `version.dll`. |
| `helixsr` | table | `{ modes = [...], preferred = "..." }`. `modes` is any of `scaler`, `fsr31_dropin`; `preferred` must be one of the listed `modes`. |
| `notes` | string | A one-line caveat the UI shows beside the section. |

`recommended` values are the `UpscalerId` set:

| Id | Meaning |
|---|---|
| `native` | Make no change; the game runs its own renderer. |
| `dlss_native` | Leave the game's own DLSS on (the RTX path — no injection). |
| `fsr4_upgrade` | Steam's FSR 4 upgrade (`PROTON_FSR4_UPGRADE`). |
| `fsr4_rdna3` | FSR 4 on RDNA 3 through the community path. |
| `dlss_to_fsr4` | Run an injected DLSS-to-FSR-4 translation. |
| `dlss_to_fsr31` | DLSS to FSR 3.1 translation. |
| `dlss_to_xess` | DLSS to XeSS translation. |
| `xess_upgrade` | Upgrade the game's own XeSS. |
| `ffx3_upgrade` | Upgrade FSR 3. |
| `ffx4_upgrade` | Upgrade FSR 4. |
| `hlx_dlss` | HelixSR's DLSS path. |
| `hlx_fsr4` | HelixSR's FSR 4 path. |
| `hlx_fsr4_drop_in` | The HelixSR FSR 4 drop-in. |
| `system_fsr` | The system FSR path. |

`dlss_native` and `native` both map to "change nothing" in the launch profile;
`dlss_native` exists so the RTX recommendation reads honestly.

### `[runtime]`

| Key | Type | Notes |
|---|---|---|
| `wrappers` | array | Suggested wrappers: `gamemoderun`, `mangohud`, `gamescope`. Suggestions only — a user's own Runtime panel wins. |
| `env` | table | A pinned environment (key → value). Usually empty. A key that is not a valid environment-variable name is rejected. |
| `known_issues` | array | One-line notes the UI can surface. |

### `[neural_rendering]`

**The section's presence is the availability flag**: a game with no
`[neural_rendering]` table has no DLSSNR route. Leave it out unless you know the
title supports it.

| Key | Type | Notes |
|---|---|---|
| `backend` | enum | `auto` (resolved from the GPU: RDNA 4 → upstream, RDNA 3 → fork), `rdna4_upstream` or `rdna3_fork` to pin one. Never offer one architecture the other's package. |
| `route` | enum | `optiscaler` only in v1. The ReShade feeder routes are post-v1 and refused by name. |
| `min_mesa` | version | `major.minor[.patch]`, e.g. `26.2` or `26.2.3`. |
| `dlssnr_min_version` | version | The minimum DLSSNR package version this entry was curated against. |
| `model_scale_default` | table | Architecture → scale, an integer in `25..=100`. The card's VRAM can still cap it. |
| `requires_64bit` | bool | Defaults to `true`; the game's own PE header is checked independently. |
| `notes` | string | The in-game key reminder (for example `End = NR Control`). |

### `[ingame]` — optional

A game may override the shipped per-mode hint **note** — the one-line caveat
beside the in-game key list — without changing the keys, which are upstream's:
`notes = "..."`. Only the note is overridable.

## Merge: shipped + user, field by field

A user file with the same `store`/`store_id` as a shipped entry **overrides only
the keys it sets**; every other field is inherited. A user file for a game the
shipped DB does not know is **added**. Merging happens on the wire types before
validation, so the merged result is validated once.

```toml
# $XDG_CONFIG_HOME/pitagames/compat/steam-1091500.toml
# Pin my own machine's recommendation; everything else still comes from shipped.
[upscaling]
recommended = { rdna1 = "native" }
```

For a shipped contribution, do not rely on inheritance from another shipped file
(the shipped layer has no inheritance) — write the fields the entry needs.

## What validation rejects

A merged entry is refused, and reported with the files that contributed to it,
when:

- `store`, `store_id` or `title` is missing or blank;
- an unknown key appears anywhere (a typo — the whole table is rejected);
- an enum value is not one of the allowed strings above;
- a version is not `major.minor[.patch]`;
- a `model_scale_default` value is outside `25..=100`;
- `neural_rendering.route` is anything but `optiscaler`;
- `helixsr.preferred` is not one of the listed `helixsr.modes`;
- a `[runtime] env` key is not a valid environment-variable name.

A duplicate within one layer (two shipped files for the same game, or two user
files) is a warning, and the later file wins.

## Rules of thumb

- **Only recommend what you can stand behind.** The two in-house cards are RDNA 4
  and RDNA 1; everything else stays `experimental` and the UI says so.
- **`anticheat` gates injection.** If a game is protected, say so, set an empty
  or native `supported`/`recommended`, and add a `known_issues` line explaining
  why — do not leave a route that would inject into a protected process.
- **`[neural_rendering]` is opt-in per game.** Its presence promises the whole
  chain (Mesa version, 64-bit, OptiScaler route, a pinned package) is viable.
- **A fix is one data file.** Keep the change minimal and let the strict loader
  tell you when you are wrong.

## Checking your change

```bash
# The list must be warning-free (warnings go to stderr).
pitagames-cli compat --shipped ./compat

# The repository's own guard, from the pure-crate tests:
cargo test -p pitagames-platform
```

The `the_repository_database_is_warning_free` test loads the real `compat/` and
asserts every entry is verified for RDNA 4; the
`the_contribution_guide_lists_every_schema_value` test asserts this page still
names every value the schema accepts.

## See also

- [`docs/m1-compat-db.md`](m1-compat-db.md) — the loader, the merge internals and
  the CLI diagnostic.
- [`docs/m3-capability.md`](m3-capability.md) — how an entry becomes an
  availability verdict, and the "why disabled" reasons.
- [`docs/dlssnr.md`](dlssnr.md) — the neural-rendering backends and the pin data.
- [`docs/eac-v2.md`](eac-v2.md) — what `eac_v2` means for a protected game.
- [`compat/`](../compat) — the shipped entries to copy from.
