# ADR-0024: Run upstream installers; never redistribute artifacts

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §6 (HelixSR, DLSSNR-AMD); `AGENTS.md` (Never redistribute; DLSSNR scope)

## Context

Two external projects provide the differentiating graphics features: **HelixSR**
(closed-source freeware; its built DLL embeds NVIDIA's network and AMD material)
and **DLSSNR-AMD** (MIT code, but the model is NVIDIA's `nvngx_dlssnr.dll`).
Both ship installers whose behavior changes between releases, and both carry
licenses that forbid redistributing built artifacts or the models.

## Decision

- **Orchestrate the official installer, unmodified, in an embedded PTY.** Run
  `helixsr-setup.sh <game_dir>` / `bash install.sh "<exe dir>" --dll <path>` and
  drive its prompts. **Never patch setup scripts** (explicitly forbidden for
  HelixSR) and **never reimplement** the installer.
- **Never redistribute** built artifacts or model weights. Do not ship
  `helixsr.dll`; do not ship or copy `nvngx_dlssnr.dll`/`dlssnr.bin`. The model
  is extracted from the **user's own** file and kept inside the mod's package
  dir; exclude it from support bundles by default; state in the UI that it must
  not be shared.
- **Capture the launch options the installer prints**: a generic `mods::capture`
  parser finds every line containing `%command%`, strips it, parses the env
  assignments, validates them against the allowlist (ADR-0023) and stores them in
  `Profile.neural_rendering.captured_env`. This removes the copy/paste step and
  works for any future mod that prints launch options.
- **Pin versions per backend; updates are opt-in, never silent** (both are 0.0.x /
  fast-moving). Hash-verify every download.
- **DLSSNR scope:** the `optiscaler` route only, x86_64 only. The backend comes
  from GPU architecture — RDNA 4 → upstream GitLab project, RDNA 3 →
  `mauri870/DLSSNR-RDNA3` fork (`experimental`) — and one architecture's package
  is never offered to the other. ReShade-feeder routes and 32-bit are post-v1.
- **Expose only a few safe keys** through the line-preserving ini patcher
  (`[HelixSR] Int8`/`WaveSize`; `[Network] Model`, `model_scale`, …); everything
  else stays in the upstream in-game menu, whose ranges change between releases.
- **Uninstall** prefers the vendor's remove path, falling back to our own file
  manifest + `.gl-backup/` restore, then a directory hash check.
- **Verify** by looking for the mod's own success marker in its log
  (`dlssnr-amd.log`, `helixsr.log`), surfaced next to upscaler verification.

## Consequences

- Positive: the launcher stays on the right side of both licenses and never
  guesses upstream behavior.
- Positive: a stalled upstream still leaves users clean (vendor uninstall + our
  manifest).
- Negative: installs need a PTY and the user's own model file, and are slower and
  more fragile than a bundled artifact would be — accepted deliberately.
- Negative: a hard incompatibility set (HelixSR and DLSSNR each ship their own
  OptiScaler) must be enforced by the single injection slot (ADR-0023).

## Alternatives considered

- **Bundle the built artifacts.** Rejected — license violation (HelixSR freeware
  limits; NVIDIA model weights).
- **Reimplement / auto-answer the installers.** Rejected — upstream behavior is
  never guessed; the prompt form is presented but the user answers.
- **Ship the RDNA 4 DLSSNR package to RDNA 3.** Rejected — the fork is
  independent and must never be silently substituted.

## References

- plan §6 *Upscaling on RDNA 1/2 — HelixSR*, *Neural rendering — DLSSNR-AMD*.
- `AGENTS.md` — *Never redistribute*, *DLSSNR scope*.
- `docs/adr/0023`, `docs/adr/0004`.
