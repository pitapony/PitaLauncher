# Tier A: the env-only OptiScaler hook (`platform::tier_a`)

Status: implemented and verified against fixture Proton trees and a fixture
`CompatEntry` on macOS. The Graphics screen that offers it is a later ticket
(#19/#24); this slice is the gate, the per-game DLL resolution and the env the
launch gets. No real Proton-CachyOS build has been driven and no real game has
launched through it.

## Why this slice exists

Tier A is Proton's own OptiScaler integration and the **default** injection path
(ADR-0023). Proton-CachyOS (and umu-protonfixes) downloads, verifies and links
the DLLs into the game's prefix itself, so the launcher's whole contribution is
two environment variables:

```text
PROTON_USE_OPTISCALER=1 PROTON_OPTISCALER_NAME=<name.dll> %command%
```

Nothing is written to the game directory, which is what makes Tier A fully
reversible: there is no manifest and nothing to uninstall. Tier B (`mods::tierb`)
is the fallback for Valve Proton / GE / an older build, or when the user wants
OptiScaler's in-game menu.

## Pieces

| Item | Role |
|---|---|
| `tier_a::USE_OPTISCALER` | `PROTON_USE_OPTISCALER`, the hook switch |
| `tier_a::OPTISCALER_NAME` | `PROTON_OPTISCALER_NAME`, the injected DLL name |
| `tier_a::CACHYOS_HOOKS_MIN` | the hook build, `11.0-20260506` |
| `tier_a::TIER_A_DLLS` | the injectable names, `dxgi → d3d12 → dbghelp` |
| `tier_a::gate(proton)` | the resolved build's verdict |
| `tier_a::dll_name(compat)` | the per-game DLL, with the ordered fallback |
| `tier_a::env(compat)` | the two-variable env vector |
| `capability::reason::TIER_A_*` | the user-facing reasons the gate maps to |
| `envc` (`Injection::EnvOnly`) | emits the env; refuses an unsupported build |

## The gate

`gate` reads the **resolved** build ([`platform::proton`](m1-proton.md)), never
the profile's `ProtonRef`: only a concrete `ProtonInstall` knows its version.

| Resolved build | `TierAGate` | Reason the UI shows |
|---|---|---|
| Proton-CachyOS ≥ `11.0-20260506` | `Supported` | — |
| Proton-CachyOS older | `TooOld` | `TIER_A_NEEDS_CACHYOS` |
| Proton-CachyOS with no readable `version` | `Unreadable` | `TIER_A_VERSION_UNREADABLE` |
| GE / Valve / umu / other | `OtherBuild` | `TIER_A_NEEDS_CACHYOS` |
| Steam decides (no resolved build) | `SteamDefault` | `TIER_A_STEAM_DEFAULT` |

An **unreadable** version is never assumed to be new enough (plan §6): it hides
Tier A exactly like an old one, with its own reason so the user knows the
difference between "upgrade Proton-CachyOS" and "we could not read this build".
`capability` owns the mapping from [`TierAGate`] to those strings — the same
strings the UI shows and [`docs/m3-capability.md`](m3-capability.md) quotes — and
`envc` refuses through `capability::availability(Field::TierAInjection, …)`, so
the matrix and the compiler cannot disagree. When Tier A is hidden the matrix
still offers `Field::TierBInjection` as the fallback.

## The DLL name

Proton injects one DLL. The name comes from the game's compat entry
(`upscaling.preferred_dll`); a preference Tier A cannot inject — `winmm.dll` or
`version.dll`, which only Tier B supports — or no preference at all falls back to
the ordered list head, `dxgi.dll`. The ordered list is
`dxgi → d3d12 → dbghelp`, the three names Proton-CachyOS accepts.

| Compat `preferred_dll` | `PROTON_OPTISCALER_NAME` |
|---|---|
| `dbghelp.dll` | `dbghelp.dll` |
| `d3d12.dll` | `d3d12.dll` |
| `winmm.dll` / `version.dll` | `dxgi.dll` (ordered fallback) |
| none / no entry | `dxgi.dll` |

## The env

`envc`'s `Injection::EnvOnly` arm emits `tier_a::env` — `PROTON_USE_OPTISCALER=1`
and `PROTON_OPTISCALER_NAME=<dll>` — through the compiler's set-semantics, so a
later entry wins and no launcher-owned key appears twice. The gate already ran
(`require(Field::TierAInjection)`), so an older or unreadable build never reaches
the emission. The env contains **no** `WINEDLLOVERRIDES`: that key exists only
because Tier B writes a DLL into the game directory, and Tier A writes nothing.

## Verification

Pure crates (macOS host and CI):

```bash
cargo test -p pitagames-platform
cargo clippy -p pitagames-platform --all-targets -- -D warnings
cargo fmt --all -- --check
```

- **Unit tests (`tier_a`)** — the gate over hand-built installs (hook build and
  newer, older, unreadable, non-CachyOS, Steam deciding); the DLL name from the
  compat entry and the ordered fallback; the env shape; and that the env sets no
  `WINEDLLOVERRIDES`.
- **Fixture trees (`tests/tier_a.rs`)** — `ProtonInstall::from_dir` over a
  directory with a `proton` script and a `version` file covers the supported,
  too-old and unreadable builds end to end; a supported tree emits the plan's env
  with the compat DLL; and the game tree is byte-for-byte unchanged across a Tier
  A decision (the "installs nothing to disk" criterion).
- **`envc`** — `Injection::EnvOnly` emits the hook and the compat DLL, falls back
  to `dxgi.dll` without a preference, and is refused with
  `TIER_A_NEEDS_CACHYOS` / `TIER_A_VERSION_UNREADABLE` / `TIER_A_STEAM_DEFAULT`.
- **Golden (`envc_golden.rs`)** — `rdna4_cachyos_tier_a_env_only` pins the env for
  a supported build.
- **`capability`** — an unreadable build hides Tier A and still offers Tier B.

## Known limits

- **Not driven against a real Proton-CachyOS build or a real game.** The env
  variable names (`PROTON_USE_OPTISCALER`, `PROTON_OPTISCALER_NAME`) and the three
  accepted DLL names are Proton-CachyOS 11's documented interface; the fixture is
  hand-built.
- **The Graphics screen is not wired.** `crates/app` is untouched; the matrix
  already reports the `TierAInjection` verdict and reason for it, but the
  selection UI is #19/#24.
- **`Injection::Auto` does not resolve to Tier A yet.** The default profile leaves
  injection `Auto`; the "resolve from the GPU/Proton capabilities" step belongs to
  the Graphics screen. An explicit `Injection::EnvOnly` is the Tier A path today.
- **Anti-cheat is not folded in here.** The injection gate
  (`platform::anticheat`) that refuses Tier A for a protected game lands with the
  Graphics screen (#24).
