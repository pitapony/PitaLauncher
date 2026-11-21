# ADR-0023: Graphics injection tiers and the single injection slot

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §6 (Injection tiers, Env compiler); `AGENTS.md` (One injection slot)

## Context

Upscaling and neural rendering are delivered by injecting DLLs / environment
into a game under Proton. Several independent projects (Proton's own OptiScaler,
a manually managed OptiScaler, HelixSR, DLSSNR-AMD) all want to own the same
`dxgi.dll`-style slot and the same env. More than one at once is a broken setup,
not a feature.

## Decision

- **Tier A — env-only (default).** Requires Proton-CachyOS ≥ 11.0-20260506 (or
  umu-profixes) with OptiScaler support; Proton deploys/validates the DLLs
  itself, config lives in the prefix. PitaGames injects env only. Fully
  reversible, nothing to uninstall.
- **Tier B — managed manual OptiScaler.** For Valve Proton / GE / older builds or
  when the user wants the in-game menu: resolve the exe dir, download + SHA-256
  verify the release, **choose a DLL name** `dxgi → d3d12 → dbghelp → winmm →
  version` *skipping names already owned* by the game or another mod, copy
  `OptiScaler.dll`/`.ini`/folder, patch the ini **line-preservingly** (comments
  and ordering survive), write a manifest with BLAKE3 hashes and `.gl-backup/`
  copies, set `WINEDLLOVERRIDES="<name>=n,b"`, and support one-click uninstall
  with a tree-hash check.
- **One injection slot.** Tier A, Tier B, `Hlx*` (HelixSR) and
  `NeuralRendering::DlssNrAmd` (DLSSNR) are **mutually exclusive** — they cannot
  coexist with each other or with a ReShade-based mod. The compiler returns a
  **typed conflict error** with a "resolve" action instead of emitting
  contradictory env. Switching asks whether to uninstall the previous one.
- **Force `PROTON_USE_OPTISCALER=0`** when DLSSNR owns `dxgi.dll`, so
  Proton-CachyOS does not inject a second OptiScaler.
- **Pre-flight scan** for conflicting files (`OptiScaler.ini`, an existing
  `dxgi.dll`, `ReShade.ini`) listing the files and their owners.
- **Detect anti-cheat → injection is disabled** with an explicit warning
  (ADR-0025); `Unknown` never hard-gates.
- `platform::envc` is the only compiler (`compile(profile, gpu, proton, game) ->
  Result<LaunchSpec>`, golden-file tested). Unknown/unsupported env keys are
  **rejected at compile time**, never silently dropped; `captured_env` (from a
  mod's printed launch options) is validated against a small allowlist
  (`WINEDLLOVERRIDES`, `VK_ADD_LAYER_PATH`, `VK_INSTANCE_LAYERS`, `DXVK_*`,
  `VKD3D_*`). Every result is shown in the UI's Command panel with a copy button
  and a diff vs. defaults.

## Consequences

- Positive: one slot means a coherent, reversible setup; contradictory env can
  never be emitted.
- Positive: no shell, allowlisted env, hash-verified downloads and line-preserving
  ini edits keep injection auditable and reversible.
- Negative: switching tiers requires uninstalling the previous one (a deliberate
  friction that prevents two OptiScalers meeting).
- Injected upscalers/frame-gen/neural rendering are refused **by name** until M3
  compiles them (`platform::envc` ships the M1 runtime subset only).

## Alternatives considered

- **Allow several injectors at once.** Rejected — DLL-name collisions and
  contradictory env produce an unexplainable broken game.
- **Always manage OptiScaler by hand (skip Tier A).** Rejected — Tier A is fully
  reversible and needs no files.
- **Advisory (not enforced) conflict rules.** Rejected — the plan's rule is
  "enforced, not advisory".

## References

- plan §6 *Injection tiers*, *Env compiler*.
- `AGENTS.md` — *One injection slot*.
- `docs/m1-compat-db.md`, `docs/m1-anticheat.md`.
- `docs/adr/0024`, `docs/adr/0025`.
