# M3.5 — the neural-rendering reference (`platform::nr`)

DLSS 5 neural rendering on AMD is DLSSNR-AMD: NVIDIA's NR network run as Vulkan
compute shaders. It is **two independent codebases with one installer UX**, and
`platform::nr` is where the launcher keeps them apart.

This is the reference page for the whole neural-rendering feature: which backend
a card gets, how it is pinned and gated, the one rule about the model, the
injection-slot conflicts, the cost data, the known issues and what is post-v1.
The three M3.5 slices carry the detail and are cross-linked below.

## Backends: two codebases, never cross-offered

| Arch | Backend | Project | Host | Status |
|---|---|---|---|---|
| RDNA 4 (RX 9000) | `rdna4_upstream` | `mochizuki0323/DLSSNR-AMD` | GitLab | **validated by us** on the RX 9070 XT |
| RDNA 3 (RX 7000) | `rdna3_fork` | `mauri870/DLSSNR-RDNA3` | GitHub | `experimental` — a Linux-only port, tested only on one card, **not validated by us** |
| RDNA 1/2, NVIDIA, Intel | — | — | — | no neural-rendering path |

A card is offered **only its own** backend, and the package is never silently
swapped for the other architecture's (cross-offer gate below, ADR-0024). The
upstream and the fork drift independently, so each is pinned separately and the
fork's lag is surfaced (`PIN_BEHIND`) rather than papered over.

The fork is the only path marked `experimental`; per ADR-0004 the UI says "not
tested by us" beside it. RDNA 4 is the path we validate on the RX 9070.

Hard requirements (gate the option, never launch-and-hope):

- RDNA 4 or RDNA 3 on Linux, with Vulkan `VK_KHR_cooperative_matrix`.
- Mesa ≥ 26.2. An unreadable Mesa version is hidden, never guessed.
- A recent Proton/GE build. NR profiles default to GE-Proton, then Valve's build,
  and Proton-CachyOS is allowed only with its OptiScaler injection disabled.
- A **64-bit** game. DLSSNR v1 ships x86_64 only, so a 32-bit game is refused.
- `bash` + `python3` for the installer and its model extraction.

## The shipped pins

The pins are **data**, not code (`crates/platform/data/dlssnr-backends.toml`,
embedded at build time). They are the "shipped data" ADR-0024 requires for every
release fetch, so an upstream tag moving cannot silently become a different
install.

| `backend` | `source` | `project` | `tag` | `version` | `asset` |
|---|---|---|---|---|---|
| `rdna4_upstream` | GitLab | `mochizuki0323/DLSSNR-AMD` | `v0.0.9` | `0.0.9` | `DLSSNR-AMD-Vulkan-Linux-0.0.9-x86_64.tar.gz` |
| `rdna3_fork` | GitHub | `mauri870/DLSSNR-RDNA3` | `v0.0.8` | `0.0.8` | `DLSSNR-AMD-Vulkan-Linux-0.0.8-x86_64.tar.gz` |

```toml
[[backends]]
backend = "rdna4_upstream"       # rdna4_upstream | rdna3_fork
source = "gitlab"                # gitlab | github
project = "mochizuki0323/DLSSNR-AMD"
tag = "v0.0.9"
version = "0.0.9"                # what the fork-lag gate compares
asset = "DLSSNR-AMD-Vulkan-Linux-0.0.9-x86_64.tar.gz"
sha256 = "…64 hex…"              # the pin we ship, never the API's digest
```

`version` is carried explicitly because the two hosts tag differently; it is what
the fork-lag gate compares against a game's
`[neural_rendering] dlssnr_min_version`.

`BackendPin::pinned()` turns a pin into the `mods::release::PinnedRelease` the
shared fetcher resolves and verifies: a byte stream whose SHA-256 is not the pin
is refused — and so is a host whose own published digest contradicts the bytes.
`BackendRegistry::shipped()` is the embedded file; `BackendRegistry::effective()`
merges a user file (`<XDG_CONFIG_HOME>/pitagames/dlssnr/backends.toml`) over it,
a user pin replacing the shipped one for the same backend, so a user can move to
a newer release without waiting for a PitaGames update. The digests ship as
placeholders until the pins are first fetched against the real hosts (a
placeholder can never install — the fetch refuses the bytes).

The registry, the pinning, the gates and the cost data all live in
`platform::nr`, which is **framework-free**: the GPU profile, the compat entry,
the resolved Proton and the game's own bitness are passed in, so the whole plan
is tested against fixture values with no GPU, no game and no network.

## The model: never redistributed

**The rule, plainly: DLSSNR's *code* is MIT, but the NR network weights are
NVIDIA's property and are in no release. PitaGames never ships, copies, mirrors
or commits them.** The model is sourced from the user's own machine, stays on the
user's own machine, and the UI says so.

The sourcing flow (`platform::nr_install`):

1. **Scan.** `scan_roots(install_dir, prefix)` + `scan()` walk the game's install
   directory and, for a non-Steam game, its Wine prefix's `drive_c` (bounded:
   depth ≤ 6, ≤ 200k entries) for `nvngx_dlssnr.dll`, verifying each file's
   **PE version resource** with `goblin`. Only `310.9.16` (preferred) and
   `310.8.0` are accepted; a fourth component (`310.9.16.0`) is tolerated.
2. **Pick.** An empty scan sets `ModelScan::offers_picker()`, and
   `inspect_model(path)` checks a file the user chooses (the panel's path field
   is the picker; this Dioxus/WebKit build has no native file dialog bound).
3. **Pass the original path.** The launcher never copies the model: `plan()`
   composes `--dll <original path>`, and `is_launcher_owned(path)` refuses a
   source inside the launcher's own XDG trees (`launcher_owned_roots()` —
   `<XDG_DATA_HOME>/pitagames` and `<XDG_CONFIG_HOME>/pitagames`). "Keep it on
   this machine" is enforced, not just stated.
4. **The package caches its own copy.** Upstream's installer extracts the weights
   into the package's own `dlssnr-amd/` directory (`package_cache_dir`), not a
   central launcher store, and a `CachedInPackage` source then needs no `--dll`.

`DO_NOT_SHARE` ("The NR model is NVIDIA's property. Keep it on this machine — do
not share, upload or commit it.") is the sentence the UI shows. The support
bundle excludes the weights by construction (`docs/m3.5-nr-verify.md`): the
collector has no field for them, so `nvngx_dlssnr.dll`, `dlssnr.bin` and
`dlssnr-small.bin` can never enter a bundle even when they sit beside an
included log.

## The seam

```rust
pub fn plan(request: NrRequest<'_>) -> NrVerdict;

pub struct NrRequest<'a> {
    pub gpu: &'a GpuProfile,
    pub profile: &'a Profile,
    pub registry: &'a BackendRegistry,
    pub compat: Option<&'a CompatEntry>,
    pub proton: Option<&'a ProtonInstall>,
    pub game_64bit: Option<bool>,   // from `pe_is_64_bit`, None when unknown
}
```

`plan` returns `NrVerdict::Available(NrPlan { backend, pin, experimental })` or
`NrVerdict::Hidden { reason }`. `NrPlan.experimental` is `true` for the RDNA 3
fork, which the UI marks "not tested by us" (ADR-0004).

## The gates, in order

1. **The capability matrix.** `plan` first calls
   [`capability::availability(Field::NeuralRendering, …)`](m3-capability.md), so
   the arch, the single injection slot (ADR-0023), game curation, the package's
   64-bit flag, Mesa and Proton are decided in exactly one place. A `Hidden`
   verdict from the matrix is returned unchanged.
2. **Cross-offer.** The requested backend — the profile's own DLSSNR choice,
   else the game's curated choice, else the card's — must match the card. An
   RDNA 3 card asked for the upstream package is `RDNA4_ONLY`; an RDNA 4 card
   asked for the fork is `RDNA3_ONLY`. Never silently swapped.
3. **The game's own bitness.** The matrix only sees the package's `pkg_arch`; the
   game binary's PE header is read by `pe_is_64_bit` and a 32-bit game is
   `NR_32BIT` (DLSSNR ships x86_64 only in v1).
4. **The pin.** The resolved backend must have a pin (`BACKEND_NOT_PINNED`
   otherwise), and its `version` must not be older than the game's
   `[neural_rendering] dlssnr_min_version` (`PIN_BEHIND`). This is the fork-lag
   gate: the RDNA 3 fork drifts behind upstream, and a game that needs the newer
   network hides the stale fork rather than installing it.

## The injection slot and file conflicts

There is **one injection slot** (ADR-0023): Tier A (Proton env), Tier B (manual
OptiScaler), HelixSR and DLSSNR are mutually exclusive. DLSSNR ships OptiScaler
unchanged and adds `dlssnr-amd.asi` as a plugin, so on the `optiscaler` route it
*owns* the upscaler — the Graphics screen shows "Handled by DLSSNR (Insert)" in
place of the launcher's upscaler rows, and the matrix reports `NR_OWNS_SLOT` /
`UPSCALER_OWNS_SLOT` for the losing choice.

`envc` folds the captured launch options into the launch environment and always
emits `PROTON_USE_OPTISCALER=0` for NR, **re-asserting it after an advanced env
override**, so the two OptiScaler copies can never meet.

- **File conflicts.** DLSSNR owns `dxgi.dll` (or another DLL) in the game
  folder; an existing `dxgi.dll` / `OptiScaler.ini` / `ReShade.ini` triggers a
  pre-flight warning listing the files and their owners, and switching injection
  modes asks whether to uninstall the previous one.
- **Anti-cheat.** A known EAC/BattlEye game disables NR with an explicit warning;
  injection is never automatic for a protected game.
- **Proton.** NR profiles default to GE-Proton, then Valve's build
  (`nr::default_proton`); `nr::proton_note` adds the CachyOS warning when the
  resolved build is Proton-CachyOS. An explicit profile choice always wins.

## Cost table and presets

The plan §6 numbers are data. `cost(backend, resolution, model_scale)` estimates
the network's frame time and VRAM; the estimate scales with the **square** of
`model_scale`, because it is a linear-resolution factor (half the network
resolution is roughly a quarter of the cost).

| Backend, 100 % | 1080p | 1440p | 4K |
|---|---|---|---|
| RDNA 4 (RX 9070 XT, `CC_SMALL_FP8`) | 3.3 ms | 5.5 ms | 12.7 ms |
| RDNA 3 (RX 7900 XTX, fork defaults) | 15 ms | 26 ms | 56 ms |

`model_scale_preset(backend, resolution, vram_bytes)` is the UI's default:

- RDNA 4 runs the full network at every resolution (≈2.6 GB at 4K).
- The RDNA 3 fork is full size up to 1440p and **half at 4K**, where the full
  network costs ≈5.3 GB and 56 ms.
- The card's VRAM then caps the preset: the scale steps down a quarter at a time
  until the network's estimated VRAM fits within **half** the card, leaving the
  other half for the game. An 8 GB RDNA 3 card is not handed the 4K full-network
  default; an unknown VRAM size applies no cap (never guess).

`default_scale(compat, …)` prefers the game's curated
`model_scale_default[arch]` when it has one, so a title can pin its own value.

## Reason strings

The exact text the UI shows, one row per `nr::REASONS` entry. A test asserts each
appears verbatim in this table. The first six are the shared
`capability::reason` strings the coarse gate returns; the last four are the
backend plan's own.

| Constant | Reason |
|---|---|
| `NR_NEEDS_RDNA3_4` | neural rendering needs an AMD RDNA 3 or RDNA 4 card |
| `NR_NEEDS_MESA` | DLSSNR needs Mesa 26.2 or newer |
| `NR_MESA_UNKNOWN` | DLSSNR needs Mesa 26.2 or newer, but the Mesa version could not be detected |
| `NR_32BIT` | DLSSNR ships x86_64 only; 32-bit games are unsupported in v1 |
| `GAME_NO_NR` | neural rendering is not curated for this game |
| `GAME_NOT_CURATED` | this game is not in the compatibility database yet |
| `RDNA4_ONLY` | the RDNA 4 upstream package is offered only on RDNA 4 |
| `RDNA3_ONLY` | the RDNA 3 fork is offered only on RDNA 3 |
| `BACKEND_NOT_PINNED` | no release is pinned for this neural-rendering backend yet |
| `PIN_BEHIND` | the pinned DLSSNR release is older than this game requires |

The same reason string is both the UI's "why disabled" copy and the docs' "why"
column; a reason edited in `crates/platform` without editing this table fails the
docs-agreement test.

## The three M3.5 slices

`platform::nr` is the registry, the pinning, the gates and the cost data. The
feature around it is three slices, each with its own page:

- **[`docs/m3.5-nr-install.md`](m3.5-nr-install.md) (ticket #29)** — the in-app
  NR installer and model sourcing: the pre-flight form, upstream's unmodified
  `install.sh` on the shared terminal with the game's executable directory as
  cwd, the `%command%` capture, the model scan / PE-version check / picker, and
  the never-guess prompt policy.
- **[`docs/m3.5-nr-verify.md`](m3.5-nr-verify.md) (ticket #31)** — verification,
  the support bundle and the in-game hints: engagement read from
  `dlssnr-amd.log` with markers as data, an opt-in allowlisted bundle that never
  carries the weights or a secret, and the per-mode in-game keys with a per-game
  note override.
- **[`docs/m3.5-nr-uninstall.md`](m3.5-nr-uninstall.md) (ticket #32)** — the
  uninstall: upstream's own `remove` route preferred, the launcher's manifest +
  `.gl-backup/` restore as the fallback, and the fallback's BLAKE3 check
  reporting a changed tree rather than restoring over it.

The ini surface, the Proton choice and the merged-env explanation (ticket #30)
are summarized in the next section.

## CLI diagnostic

```bash
pitagames-cli nr                                     # this machine, no game
pitagames-cli nr --game steam 1091500                # with a curated entry
pitagames-cli nr --root /path/to/fixture/sys --json  # a fixture GPU tree
pitagames-cli nr --resolution 4k --vram 8            # the preset and cost
pitagames-cli nr --fetch /path/to/fixture --dest /tmp/nr   # verify the pin offline
```

```
GPU: AMD Radeon RX 9070 XT (RDNA 4 (RX 9000))
Backend: rdna4_upstream (GitLab mochizuki0323/DLSSNR-AMD @ v0.0.9)
Pin: DLSSNR-AMD-Vulkan-Linux-0.0.9-x86_64.tar.gz
Experimental: no
Model scale: 100 % at 4K (≈12.7 ms, 2.6 GB)
```

`--json` prints `{ "gpu": …, "verdict": …, "resolution": …, "model_scale": …,
"cost": … }`. `--fetch <dir>` loads the resolved pin's `release.json` and asset
from a fixture directory, so the whole fetch → verify → unpack runs offline.

## The ini surface, the Proton choice and the merged env

`platform::nr_ini` is the safe `dlssnr-amd.ini` surface (plan §7). The whole
allowlist is `IniSurface::dlssnr()` — `[Network] Model`/`model_scale`,
`[Int4Mixed] Enabled`, `[Preprocess] Enabled`/`Exposure`/`Curve` and
`[NRControl] MenuKey` — and `settings(model, model_scale)` composes only the keys
the launcher has a selection for:

| Key | Written from |
|---|---|
| `[Network] Model` | the network (`CC_SMALL_FP8`, `CC_SILVER_AARDWOLD`, `CC_SILVER_AARDWOLD_INT4_MIXED`, `CC_SMALL_FP8_EXPERIMENTAL`) |
| `[Network] model_scale` | the `model_scale` selection (25..=100) |
| `[Int4Mixed] Enabled` | whether the network is int4 mixed |
| `[NRControl] MenuKey` | `End`, upstream's default |

`[Preprocess]` is on the surface but is not written by a plain install: it
changes the picture NR is shown, so it stays in the in-game menu unless the user
changes it there. `patch` is the shared line-preserving patcher, so comments,
ordering and CRLF survive and a key the surface does not allow is refused by name.

`ShimProfile::explain_lines` is the one renderer behind `pitagames-shim --explain`
and `pitagames-cli mods explain <store> <id> [--dir <path>] [--json]`: the Proton
it resolved, the merged environment (which already includes the captured options),
the captured launch options named separately, and the wrapper chain.

## Verification

Pure crates (macOS host and CI):

```bash
cargo test -p pitagames-platform -p pitagames-cli
cargo clippy -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`nr` covers: the shipped registry pinning both backends and refusing a bad
digest; a user file overriding a shipped pin; each backend fetching and
verifying its pinned fixture asset (and a mismatch being refused); the
cross-offer rule both ways; a card with no NR path; the plan resolving the
upstream on RDNA 4 and the experimental fork on RDNA 3; the fork-lag gate; a
32-bit game; a missing pin; an uncurated game; an unreadable Mesa; the cost
table at full scale and its square scaling; the per-resolution/GPU presets and
the small-card cap; the curated-scale override; the PE header check on a
synthetic PE32 and PE32+; the JSON round trip; and this page quoting every
reason string and naming every shipped pin.

## Known issues

- **Not run against a real release or GPU.** The digests ship as placeholders
  until the pins are first fetched against the real hosts, because the live HTTP
  `Fetcher` is still ticket #12's open item; `pitagames-cli nr` on the RX 9070 is
  the check once a target machine is reachable. `mods::release` refuses a byte
  stream that does not match the pin, so a placeholder can never install.
- **The cost figures are the plan's estimates, not measurements.** The per-scale
  scaling is a model (cost ∝ `model_scale²`), and the 50 % 4K fork figure lands
  near the plan's ≈16 ms rather than on it.
- **32-bit detection reads the game's PE header, not the package's.** A native
  Linux game has no NR path, so `pe_is_64_bit` treats a non-PE file as an error
  rather than guessing.
- **The RDNA 3 fork is `experimental` and not validated by us.** It is tested
  only on one card upstream, it drifts behind the upstream release, and a game
  that needs a newer network simply hides it (`PIN_BEHIND`) — "fork not updated"
  is unsupported, not broken.
- **The installer panel writes no launcher manifest.** It runs upstream's
  `install.sh`, which leaves only upstream's own `dlssnr-amd-install.txt` ledger,
  so the primary uninstall path is upstream's `remove`; the manifest fallback is
  the launcher-managed path (`docs/m3.5-nr-uninstall.md`).
- **Upstream's `remove` is not byte-for-byte.** It deletes what it installed and
  does not put back a game file it overwrote; only the launcher manifest promises
  that.
- **The picker is a path field, not a native file dialog** — this Dioxus/WebKit
  build has no file-dialog binding wired.
- **A Steam game's Wine prefix is not scanned** for the model; a Steam game is
  covered by its install directory (the non-Steam per-game prefix is scanned via
  `platform::prefix`).
- **`--model` names are our mapping onto upstream's flag**; if upstream renames
  one, the installer's own prompt is still surfaced, so this is a non-fatal
  mismatch.
- **The NR installer/uninstaller buttons are pointer/tab reachable but not on
  the Graphics screen's gamepad focus graph yet.**
- **The support bundle is written locally**: there is no upload and no redaction
  pass beyond the allowlist (M4).
- **The verification markers and the in-game keys are a first pass** from
  upstream's log vocabulary and the plan; a wrong one is a data edit
  (`docs/m3.5-nr-verify.md`).
- **Not tested on a TV and not validated on hardware** (ADR-0004). RDNA 4 is the
  path we validate; RDNA 3, RDNA 2, NVIDIA and Intel ship `experimental`
  /unverified with no parity claims.

## Post-v1

Explicitly out of scope for v1, hidden behind a "post-v1" note rather than
silently substituted:

- **The ReShade feeder routes** — `reshade_feeder`, `reshade_feeder_vulkan`,
  `reshade_feeder_dx9` (a game with no upscaler hook, a Vulkan layer under DXVK,
  and DX9). `NeuralRendering::validate` and `InstallForm::validate` refuse them,
  and `platform::nr_install` only ever installs the `optiscaler` route.
- **32-bit (i686) NR packages.** v1 ships x86_64 only; a 32-bit game is hidden
  with `NR_32BIT`.
- **The live HTTP fetcher** (ticket #12's open item). v1's tests and
  `pitagames-cli nr --fetch` drive the fetch → verify → unpack pipeline from a
  fixture directory with no network.
- **The RDNA 3 int4 Vulkan layer.** v1 exposes only the `[Int4Mixed]` ini
  switch; the layer itself is post-v1.
- **The full upstream ini surface.** v1 exposes only the shipped allowlist
  above; everything else stays in upstream's in-game NR Control menu because its
  ranges and defaults change between 0.0.x releases.
- **Opt-in, pinned NR updates.** 0.0.x is explicitly unstable and breaks setups
  between releases, so updates ride the same opt-in rule as Proton/GE rather than
  a silent fetch.
- **RDNA 3 NR and HelixSR on RDNA 2/3 validated with community hardware**, and a
  generic PTY for third-party mod installers.
