# ADR-0031: EAC Linux v2 readiness and the boot-chain gate

- **Status:** Accepted
- **Date:** 2026-11-26
- **Deciders:** julienpohie
- **Source:** spec #8 / ticket #26; the CachyOS Easy Anti-Cheat (Linux v2) wiki;
  extends ADR-0004 (claims), ADR-0023 (injection slot) and ADR-0025 (read-only
  anti-cheat data)

## Context

Until 2026-11-22 the launcher had one honest answer for an anti-cheat game:
"protected, unsupported on Linux". CachyOS then shipped **Easy Anti-Cheat Linux
v2**, which only lets a game start when it can verify how the machine booted:

- Secure Boot enabled, with a kernel signed by a key Epic trusts;
- a TPM 2.0 that records the boot;
- the kernel in lockdown integrity mode, loading only signed modules;
- the IOMMU enabled, kexec disabled and the BPF LSM active.

CachyOS provides a dedicated kernel (`linux-cachyos-eac`) and a CachyOS-signed
systemd-boot with its own boot entry. The user keeps their own Secure Boot keys;
the only firmware change is the CachyOS EAC CA. `eac-setup` sets the system up
and `eac-setup boot` reboots into the entry once; `eac-status` verifies it.

Two facts shape the launcher's role. First, **the boot chain is the user's and
CachyOS's, not the launcher's**: PitaGames must never enroll, sign or restore
Secure Boot keys, call `sbctl`, touch the TPM or write an EFI variable. Second,
**whether Epic has enabled EAC Linux v2 for a game cannot be inferred from a
game directory**, so it has to be curated like the rest of the compat DB.

## Decision

- **`platform::eac` reads the boot chain and reports readiness.** It parses
  `/proc/cmdline` as whole tokens (the CachyOS `EAC_REQ`: `lockdown=integrity`,
  `lsm=…,lockdown,…,integrity,…,bpf`, `iommu.passthrough=0`, `intel_iommu=on`,
  `module.sig_enforce=1`, `sysctl.kernel.kexec_load_disabled=1`), the kernel
  release (`linux-cachyos-eac`), the Secure Boot/SetupMode EFI variables, the
  TPM version and the IOMMU directory. It **never calls `eac-status`**: a missing
  verification tool cannot change the answer. Everything host-specific is behind
  an injected `EacProbe`, so the verdict is fixture-tested and the crate builds
  on macOS.
- **The game claim is curated.** `platform::compat` gains an `eac_v2` boolean
  (`[game] eac_v2 = true`), defaulting off, merged field-by-field like
  `anticheat`. Folding the claim with readiness gives exactly three states:
  *protected & unsupported* (not EAC v2 — still cannot run on Linux),
  *EAC v2 & ready*, and *EAC v2 & needs the EAC entry*.
- **One guidance action, consent-gated.** When a game needs the entry, the
  detail page explains the requirement and offers "reboot into the EAC entry and
  play", which runs the argv-only `pkexec eac-setup boot` **only after an
  explicit confirmation** (`pkexec` gives it a polkit authentication). When
  `eac-setup` is absent, the launcher does not offer the action and shows
  `sudo eac-setup boot` instead.
- **The launcher never modifies the boot chain.** No `sbctl`, no key enrollment,
  signing or restore, no TPM, no direct EFI-variable write. A test asserts the
  reboot argv mentions none of those tools.
- **Injection stays hard-off** for any protected game, EAC v2 or not, even on
  the EAC entry (ADR-0023/ADR-0025 unchanged).
- **The claim ships honest.** The EAC v2 states carry "new — being tested; not
  validated by us" (ADR-0004) until a real game runs on hardware.
- **The limits are documented**: DKMS modules and kernel-level overlays do not
  work in the EAC entry (NVIDIA/ZFS/r8125 ship as `linux-cachyos-eac-*`
  packages), and the v1.1 console session must surface the same guidance because
  a gamescope session boots a normal entry.

## Consequences

- Positive: an EAC game owner gets the truth — *possible, if you reboot into the
  EAC entry* — instead of a dead end, and the launcher never risks the user's
  boot chain.
- Positive: readiness is derived from the boot itself, so it is correct even
  without `eac-status` installed, and it is fixture-tested with no EAC machine.
- Positive: the three-state model keeps the honest "not supported" answer for
  games Epic has not enabled, so EAC v2 does not become a blanket promise.
- Negative: the game claim is hand-curated, so a game Epic enables later needs a
  compat entry before the launcher offers the reboot.
- Negative: the reboot needs polkit and reboots the machine; it is deliberately a
  confirmation dialog, never automatic.
- Not validated on hardware: no EAC v2 game has run on the RX 9070 / RX 5700 XT.

## Alternatives considered

- **Run `eac-setup` (setup) from the launcher, or sign/enroll keys.** Rejected —
  it modifies the boot chain and Secure Boot keys, which is CachyOS's and the
  user's, and a launcher must never take that risk.
- **Use `eac-status`'s output as the verdict.** Rejected — it makes readiness
  depend on an optional tool, and `eac-status` can be absent on a valid boot.
- **Infer the EAC v2 claim from the game directory.** Rejected — the directory
  says nothing about Epic's server-side enablement.
- **Offer injection anyway on the EAC entry.** Rejected — the EAC entry removes
  the *kernel* obstacle, not the anti-cheat ban risk (ADR-0025).

## References

- CachyOS wiki, *Easy Anti-Cheat (Linux v2)*; CachyOS-Settings `eac-setup`.
- `AGENTS.md` — anti-cheat, the compat DB, compatibility claims.
- `docs/eac-v2.md`, `docs/m1-anticheat.md`, `docs/m1-compat-db.md`.
- `docs/adr/0004`, `docs/adr/0023`, `docs/adr/0025`.
