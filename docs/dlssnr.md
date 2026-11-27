# M3.5 — the neural-rendering backends (`platform::nr`)

DLSS 5 neural rendering on AMD is DLSSNR-AMD: NVIDIA's NR network run as Vulkan
compute shaders. It is **two independent codebases with one installer UX**, and
`platform::nr` is where the launcher keeps them apart.

| Arch | Backend | Host | Status |
|---|---|---|---|
| RDNA 4 (RX 9000) | `rdna4_upstream` — `mochizuki0323/DLSSNR-AMD` | GitLab | validated by us on the RX 9070 XT |
| RDNA 3 (RX 7000) | `rdna3_fork` — `mauri870/DLSSNR-RDNA3` | GitHub | `experimental` — a Linux-only port, tested only on one card, **not validated by us** |
| RDNA 1/2, NVIDIA, Intel | — | — | no neural-rendering path |

Each backend is pinned **separately** in shipped data (`crates/platform/data/dlssnr-backends.toml`)
and a card is never offered the other architecture's package (ADR-0024). The
module is framework-free: the GPU profile, the compat entry, the resolved Proton
and the game's own bitness are all passed in, so the whole plan is tested against
fixture values with no GPU, no game and no network. The live HTTP fetcher is
ticket #12's open item; the tests and `pitagames-cli nr` drive the
fetch → verify → unpack pipeline from a fixture.

## Pieces

| Crate | Responsibility |
|---|---|
| `platform` (`pitagames-platform`) | `BackendPin`/`BackendRegistry`, `backend_for`/`resolve`, `plan`/`NrVerdict`, `Resolution`/`Cost`/`cost`, `model_scale_preset`/`default_scale`, `pe_is_64_bit` |
| `mods` (`pitagames-mods`) | `release::PinnedRelease`/`Fetcher` — the fetch, the SHA-256 pin check and the unpack |
| `cli` | `pitagames-cli nr` diagnostic (text + `--json`) |

## Pins

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

`BackendPin::pinned()` turns a pin into the `mods::release::PinnedRelease` the
shared fetcher resolves and verifies: a byte stream whose SHA-256 is not the pin
is refused, and so is a host whose own published digest contradicts the bytes.
`BackendRegistry::shipped()` is the embedded file; `BackendRegistry::effective()`
merges a user file (`<XDG_CONFIG_HOME>/pitagames/dlssnr/backends.toml`) over it,
a user pin replacing the shipped one for the same backend.

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
synthetic PE32 and PE32+; the JSON round trip; and this table quoting every
reason string.

## Known limitations

- **Not run against a real release or GPU.** The shipped digests are
  placeholders until the pins are first fetched against the real hosts (ticket
  #12), and `pitagames-cli nr` on the RX 9070 is the check once a target machine
  is reachable. `mods::release` refuses a byte stream that does not match the
  pin, so a placeholder can never install.
- **The cost figures are the plan's estimates, not measurements.** The per-scale
  scaling is a model (cost ∝ `model_scale²`), and the 50 % 4K fork figure lands
  near the plan's ≈16 ms rather than on it.
- **The installer run and the model sourcing ship** (`docs/m3.5-nr-install.md`,
  ticket #29): the pre-flight form, upstream's `install.sh` run in the shared
  terminal with the game's executable directory as cwd, the `%command%` capture,
  and the model scan / PE-version check / picker. **The verification, the support
  bundle and the in-game hints ship too** (`docs/m3.5-nr-verify.md`, ticket #31):
  engagement read from `dlssnr-amd.log` with markers as data, an opt-in
  allowlisted support bundle, and the per-mode in-game key hints with a per-game
  note override. Still later slices: the safe ini surface (#30) and the uninstall
  (#32), and the live fetch (ticket #12). `platform::nr` itself is the registry,
  the pinning, the gates and the cost data; the route is fixed to `optiscaler` by
  the profile validation.
- **32-bit detection reads the game's PE header, not the package's.** A native
  Linux game has no NR path, so `pe_is_64_bit` treats a non-PE file as an error
  rather than guessing.
