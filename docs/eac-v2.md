# EAC Linux v2 readiness and the boot-chain gate (`platform::eac`)

**Status of the feature: new — being tested; not validated by us.** The
mechanism ships and is fixture-tested, but no real EAC game has run through it on
our hardware yet (ADR-0004). The claim is honest: PitaGames can *tell you what
this boot is missing and take you to the CachyOS EAC entry*, and nothing more.

CachyOS shipped **Easy Anti-Cheat Linux v2** on 2026-11-22. Before it, an
EAC-protected game was simply unsupported on Linux, and the launcher could only
say "anti-cheat, unsupported". EAC Linux v2 changes that: it lets a game start
when it can verify how the machine booted, and CachyOS provides a dedicated
kernel (`linux-cachyos-eac`) and a CachyOS-signed systemd-boot with its own boot
entry to make that possible. You keep your own Secure Boot keys; the only
firmware change is the CachyOS EAC CA.

This slice supersedes the plan §6 anti-cheat row, which knew only the old
"protected → unsupported" answer. See ADR-0031.

## What EAC Linux v2 verifies

EAC Linux v2 checks the boot chain, not the launcher. `platform::eac::probe`
reads the same things and reports whether this boot satisfies them:

| Requirement | id | What it means |
|---|---|---|
| Secure Boot | `secure_boot` | Secure Boot is enabled and the kernel is signed with a key Epic trusts |
| TPM 2.0 | `tpm2` | A TPM 2.0 is present and records the boot |
| EAC kernel | `eac_kernel` | The `linux-cachyos-eac` kernel is running |
| lockdown integrity | `lockdown_integrity` | The kernel runs in lockdown integrity mode |
| signed modules only | `signed_modules` | Only modules signed at build time load (`module.sig_enforce=1`) |
| kexec disabled | `kexec_disabled` | `sysctl.kernel.kexec_load_disabled=1` |
| BPF LSM | `bpf_lsm` | `lsm=` lists `lockdown`, `integrity` and `bpf` |
| IOMMU | `iommu` | The IOMMU is enabled (`iommu.passthrough=0` with an `iommu=on` option) |

The command line options are matched as **whole tokens**, so `lockdown=integrity`
must be its own option and never a substring of another. The exact set is the
CachyOS `eac-setup` `EAC_REQ`:

```
lockdown=integrity lsm=landlock,lockdown,yama,integrity,bpf iommu.passthrough=0 \
intel_iommu=on module.sig_enforce=1 sysctl.kernel.kexec_load_disabled=1
```

`platform::eac::probe` derives readiness from `/proc/cmdline`, the kernel
release (`/proc/sys/kernel/osrelease`), the Secure Boot / SetupMode EFI
variables, the TPM version, the IOMMU directory and the lockdown file. It
**never calls `eac-status`**: a missing verification tool cannot change the
readiness answer. `eac-setup`/`eac-status` presence is reported separately,
because `eac-setup` is what the reboot action needs.

## The three states

A launcher cannot infer from a game directory whether Epic has enabled EAC Linux
v2 for it, so the game is **curated**: `platform::compat`'s `eac_v2` field
(`[game] eac_v2 = true`) marks a game Epic has enabled it for. Folding the
curated claim with the host's readiness gives exactly three states
(`platform::eac::verdict`):

- **protected & unsupported** — the game uses Easy Anti-Cheat, but not EAC Linux
  v2. It cannot run on Linux yet and rebooting would not help.
- **EAC v2 & ready** — EAC Linux v2, and this boot meets every requirement. The
  game can start.
- **EAC v2 & needs the EAC entry** — EAC Linux v2, but this is not the EAC
  entry. The launcher names the missing requirements and offers the reboot.

A game with no EAC at all is `NotProtected` and shows nothing.

## The one action: reboot into the EAC entry

When a game needs the EAC entry, the detail page explains the requirement and
offers **reboot into the EAC entry and play**. The action runs the argv-only
command

```
pkexec eac-setup boot
```

and only after an explicit confirmation. `pkexec` gives the action a polkit
authentication (it reboots and needs root), and `eac-setup boot` is the **only**
boot-chain command the launcher ever runs.

The launcher **never modifies the boot chain**: it never calls `sbctl`, never
enrolls, signs or restores Secure Boot keys, never touches the TPM, and never
writes an EFI variable itself. That is CachyOS's `eac-setup`'s job, and a test
asserts the reboot argv mentions none of those tools. When `eac-setup` is not
installed, the launcher does not offer the reboot at all and shows the command
(`sudo eac-setup boot`) for the user to run themselves.

## Injection is hard-off

A game with an EAC v2 claim is still anti-cheat protected, so the injection gate
(ADR-0025) refuses injection exactly as before — even on the EAC entry. Nothing
in this slice injects, and no EAC v2 game is offered an upscaler or neural
rendering.

## Limits the user must know

- **DKMS modules and overlays do not work in the EAC entry.** The EAC kernel
  only loads modules signed when it was built, so NVIDIA, ZFS and r8125 ship as
  `linux-cachyos-eac-*` packages and other DKMS modules (for example
  `virtualbox-host-dkms`) will not load there. Gamescope and MangoHud overlays
  are user-space and still work; kernel-level mods are not.
- **Whether a game starts is still up to Easy Anti-Cheat and the game.** Meeting
  every requirement is necessary, not sufficient.
- **The v1.1 console session must check this.** A gamescope console session boots
  into a normal entry by default, so an EAC v2 game from the console shell still
  needs the EAC entry; the session work (v1.1) has to surface the same guidance.

## CLI diagnostic

```bash
pitagames-cli eac                       # probe this boot
pitagames-cli eac --cmdline ./cmdline   # probe a fixture command line
pitagames-cli eac --root ./fixture-sys  # read boot state from a fixture tree
pitagames-cli eac 1172470               # add the game's verdict (Apex Legends)
pitagames-cli eac 1172470 --json
```

The text report lists every requirement with its outcome and what was observed,
then the game's verdict. `--json` prints `{ readiness, verdict }` (or the
readiness alone without a game).

## GUI

The game detail page shows an **EAC Linux v2** card next to the Anti-cheat card
when a game is protected: the state, the missing requirements and, when the game
needs the entry and `eac-setup` is present, a **Reboot into the EAC entry and
play** button behind a confirmation dialog. The card carries the
"new — being tested; not validated by us" note. The framework-free model lives
in `crates/app/src/eac.rs`, so the copy is unit-tested without a webview.

## Verification

Pure crates (macOS host and CI):

```bash
CARGO_HOME=~/.cargo CARGO_HTTP_MULTIPLEXING=false \
  cargo test -p pitagames-platform -p pitagames-cli -p pitagames-core
CARGO_HTTP_MULTIPLEXING=false cargo clippy -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-platform` adds the `eac` tests: a full EAC command line is ready; a
normal CachyOS boot is not ready and names every gap; each requirement fails on
its own; command-line options are matched as whole tokens; readiness never
depends on `eac-status`; a missing command line warns and is never ready; the
verdict distinguishes the three states; EAC v2 without `eac-setup` cannot offer
the reboot; and the reboot argv is only `pkexec eac-setup boot` and mentions no
key/signing tool. A docs-agreement test reads this file and asserts every
requirement label and id appears verbatim, so the UI and the table cannot drift.

The GUI is verified in `archlinux:latest`: `cargo build -p pitagames`,
`cargo clippy -p pitagames --all-targets -- -D warnings` and
`cargo test -p pitagames`.

## Known limitations

- **Not hardware-validated.** No EAC v2 game has been run on the RX 9070 /
  RX 5700 XT yet. The probe is fixture-tested; the real machine is the gate.
- **No "play after reboot" memory.** The launcher offers the reboot but does not
  remember to start the game on the next boot; you launch it again once you are
  in the EAC entry.
- **Key trust is not checkable by us.** Secure Boot being enabled is all we can
  see; whether the kernel is signed with a key Epic trusts is EAC's judgement.
- **Curation is the claim.** `eac_v2` is hand-curated in `compat/*.toml`, exactly
  like `anticheat`; a game Epic enables later needs a compat entry.

## Deliberately not in this slice

- Running `eac-setup` (the setup, not just `boot`); the launcher never modifies
  the boot chain.
- Signing, enrolling or restoring Secure Boot keys, or touching the TPM.
- Auto-launching the game after the reboot.
- Inferring the EAC v2 claim from a game directory.
