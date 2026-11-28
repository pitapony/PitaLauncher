//! EAC Linux v2 readiness and the boot-chain gate.
//!
//! Since CachyOS shipped **Easy Anti-Cheat Linux v2** (2026-11-22), an
//! EAC-protected game no longer means "unsupported on Linux". EAC Linux v2 only
//! lets a game start when it can verify how the machine booted, so CachyOS
//! provides a dedicated kernel (`linux-cachyos-eac`) and a signed systemd-boot,
//! with its own boot entry. The user keeps their own Secure Boot keys; the only
//! firmware change is the CachyOS EAC CA.
//!
//! The launcher's job is narrow and honest:
//!
//! - **Read the boot chain** ([`probe`]) and say whether this boot satisfies
//!   every EAC Linux v2 requirement. The probe reads `/proc/cmdline`, the
//!   kernel release and a handful of `/proc` and `/sys` files; it never calls
//!   `eac-status`, so a missing verification tool cannot change the answer.
//! - **Curate the game** (`eac_v2` in `platform::compat`) — no launcher can
//!   infer from a game directory whether Epic has enabled EAC Linux v2 for it.
//! - **Distinguish three states** ([`verdict`]): *protected & unsupported*,
//!   *EAC v2 & ready*, and *EAC v2 & needs the EAC entry*.
//! - **Offer one action**, and only after explicit confirmation: reboot into
//!   the EAC entry by running `eac-setup boot` ([`reboot_argv`]). The launcher
//!   never touches Secure Boot keys, `sbctl`, the TPM or signing — that is
//!   CachyOS's `eac-setup`'s job, and ADR-0031 records the boundary.
//!
//! Like [`crate::gpu`] and [`crate::proton`], everything host-specific is
//! behind an injected [`EacProbe`], so the whole verdict is tested against
//! fixture command lines and boot reads with no EAC machine, and the crate
//! builds on macOS where `/proc` and `/sys` do not exist.

use std::path::Path;

use serde::{Deserialize, Serialize};

use pitagames_core::AntiCheat;

/// The program that sets up and reboots into the EAC entry.
pub const EAC_SETUP: &str = "eac-setup";

/// The program that verifies an EAC setup (informational only — never the
/// source of our verdict).
pub const EAC_STATUS: &str = "eac-status";

/// The marker CachyOS's dedicated EAC kernel carries in its release string.
pub const EAC_KERNEL_MARKER: &str = "cachyos-eac";

/// The polkit launcher the reboot action runs through, so the command needs an
/// explicit authentication and never a password on a command line.
pub const PKEXEC: &str = "pkexec";

/// The honest claim for the whole feature (ADR-0004): the mechanism ships, but
/// no real game has run through it on our hardware yet.
pub const NOT_VALIDATED: &str = "new — being tested; not validated by us";

/// Where [`EacProbe::host`] reads the command line and boot state.
pub const PROC_CMDLINE: &str = "/proc/cmdline";
pub const PROC_OSRELEASE: &str = "/proc/sys/kernel/osrelease";
pub const EFIVARS_DIR: &str = "/sys/firmware/efi/efivars";
pub const TPM_VERSION_FILE: &str = "/sys/class/tpm/tpm0/tpm_version_major";
pub const IOMMU_DIR: &str = "/sys/class/iommu";
pub const LOCKDOWN_FILE: &str = "/sys/kernel/security/lockdown";

// ---------------------------------------------------------------------------
// Requirements
// ---------------------------------------------------------------------------

/// One thing EAC Linux v2 verifies about how the machine booted.
///
/// The set is the CachyOS wiki's list: Secure Boot, a TPM 2.0, the EAC kernel,
/// `lockdown=integrity`, signed modules only, kexec disabled, the BPF LSM, and
/// IOMMU. [`Requirement::ALL`] is the order the UI and the docs table use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    SecureBoot,
    Tpm2,
    EacKernel,
    LockdownIntegrity,
    SignedModules,
    KexecDisabled,
    BpfLsm,
    Iommu,
}

impl Requirement {
    /// Every requirement, in the order the UI and the docs present them.
    pub const ALL: &'static [Requirement] = &[
        Requirement::SecureBoot,
        Requirement::Tpm2,
        Requirement::EacKernel,
        Requirement::LockdownIntegrity,
        Requirement::SignedModules,
        Requirement::KexecDisabled,
        Requirement::BpfLsm,
        Requirement::Iommu,
    ];

    /// The stable identifier used in JSON and the docs table.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SecureBoot => "secure_boot",
            Self::Tpm2 => "tpm2",
            Self::EacKernel => "eac_kernel",
            Self::LockdownIntegrity => "lockdown_integrity",
            Self::SignedModules => "signed_modules",
            Self::KexecDisabled => "kexec_disabled",
            Self::BpfLsm => "bpf_lsm",
            Self::Iommu => "iommu",
        }
    }

    /// The human label the UI and the docs table show.
    pub const fn label(self) -> &'static str {
        match self {
            Self::SecureBoot => "Secure Boot",
            Self::Tpm2 => "TPM 2.0",
            Self::EacKernel => "EAC kernel",
            Self::LockdownIntegrity => "lockdown integrity",
            Self::SignedModules => "signed modules only",
            Self::KexecDisabled => "kexec disabled",
            Self::BpfLsm => "BPF LSM",
            Self::Iommu => "IOMMU",
        }
    }

    /// The one-line "how to get there", shown next to an unmet requirement.
    pub const fn fix(self) -> &'static str {
        match self {
            Self::SecureBoot => "enable Secure Boot in the firmware",
            Self::Tpm2 => "enable the TPM 2.0 in the firmware",
            Self::EacKernel => "boot the linux-cachyos-eac kernel (eac-setup boot)",
            Self::LockdownIntegrity => "the EAC entry sets lockdown=integrity",
            Self::SignedModules => "the EAC entry sets module.sig_enforce=1",
            Self::KexecDisabled => "the EAC entry sets sysctl.kernel.kexec_load_disabled=1",
            Self::BpfLsm => "the EAC entry lists bpf in lsm=",
            Self::Iommu => "enable IOMMU in the firmware and boot the EAC entry",
        }
    }
}

/// One requirement's outcome, with what the probe observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementCheck {
    pub requirement: Requirement,
    pub satisfied: bool,
    /// What was seen (`"enabled"`, `"not in /proc/cmdline"`, …).
    pub detail: String,
}

// ---------------------------------------------------------------------------
// The injected probe
// ---------------------------------------------------------------------------

/// Everything [`probe`] reads, injected so tests need no EAC machine.
///
/// [`EacProbe::host`] fills this from the running machine; a test (or the CLI's
/// `--cmdline`/`--root`) builds it from fixture values instead. A `None` field
/// means "could not read", which is never mistaken for a satisfied check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EacProbe {
    /// `/proc/cmdline`, or `None` when it could not be read.
    pub cmdline: Option<String>,
    /// `/proc/sys/kernel/osrelease`, for the kernel flavour.
    pub osrelease: Option<String>,
    /// The `SecureBoot` EFI variable: `Some(true)` = enabled.
    pub secure_boot: Option<bool>,
    /// The `SetupMode` EFI variable: `Some(true)` = the firmware is in setup
    /// mode, so Secure Boot cannot be enforcing.
    pub setup_mode: Option<bool>,
    /// The TPM's major version (`/sys/class/tpm/tpm0/tpm_version_major`).
    pub tpm_version: Option<u32>,
    /// Whether `/sys/class/iommu` has any device under it.
    pub iommu: Option<bool>,
    /// `/sys/kernel/security/lockdown` (`[integrity] confidentiality none`).
    pub lockdown: Option<String>,
    /// Whether `eac-setup` resolves on `PATH`.
    pub eac_setup: bool,
    /// Whether `eac-status` resolves on `PATH`.
    pub eac_status: bool,
}

impl EacProbe {
    /// A probe with an explicit command line and nothing else.
    ///
    /// This is the fixture constructor: every boot read is `None`/`false` until
    /// the `with_*` builders set it.
    pub fn at(cmdline: impl Into<String>) -> Self {
        Self {
            cmdline: Some(cmdline.into()),
            ..Self::default()
        }
    }

    /// Probe the running machine.
    ///
    /// Reads `/proc/cmdline`, the kernel release, the Secure Boot EFI
    /// variables, the TPM version, the IOMMU directory, the lockdown file and
    /// resolves `eac-setup`/`eac-status` on `PATH`. Every read is best-effort:
    /// a missing file is `None`, never an error, so this works on a machine
    /// (or an OS) without any of them.
    pub fn host() -> Self {
        Self {
            cmdline: read_trimmed(Path::new(PROC_CMDLINE)),
            osrelease: read_trimmed(Path::new(PROC_OSRELEASE)),
            secure_boot: read_efivar(Path::new(EFIVARS_DIR), "SecureBoot"),
            setup_mode: read_efivar(Path::new(EFIVARS_DIR), "SetupMode"),
            tpm_version: read_trimmed(Path::new(TPM_VERSION_FILE))
                .and_then(|text| text.parse().ok()),
            iommu: read_dir_nonempty(Path::new(IOMMU_DIR)),
            lockdown: read_trimmed(Path::new(LOCKDOWN_FILE)),
            eac_setup: on_path(EAC_SETUP),
            eac_status: on_path(EAC_STATUS),
        }
    }

    /// Read the boot state from a fixture tree that mirrors `/`.
    ///
    /// This is what the CLI's `--root` uses: `<root>/proc/cmdline`,
    /// `<root>/proc/sys/kernel/osrelease`, `<root>/sys/firmware/efi/efivars`,
    /// `<root>/sys/class/tpm/tpm0/tpm_version_major`, `<root>/sys/class/iommu`
    /// and `<root>/sys/kernel/security/lockdown`. Tool presence is a host
    /// concern, so a fixture run reports both tools absent unless a caller sets
    /// them (the CLI's `--tools`).
    #[must_use]
    pub fn from_root(root: &Path) -> Self {
        Self {
            cmdline: read_trimmed(&root.join("proc/cmdline")),
            osrelease: read_trimmed(&root.join("proc/sys/kernel/osrelease")),
            secure_boot: read_efivar(&root.join("sys/firmware/efi/efivars"), "SecureBoot"),
            setup_mode: read_efivar(&root.join("sys/firmware/efi/efivars"), "SetupMode"),
            tpm_version: read_trimmed(&root.join("sys/class/tpm/tpm0/tpm_version_major"))
                .and_then(|text| text.parse().ok()),
            iommu: read_dir_nonempty(&root.join("sys/class/iommu")),
            lockdown: read_trimmed(&root.join("sys/kernel/security/lockdown")),
            eac_setup: false,
            eac_status: false,
        }
    }

    /// Replace the command line (the CLI's `--cmdline`).
    #[must_use]
    pub fn with_cmdline(mut self, cmdline: impl Into<String>) -> Self {
        self.cmdline = Some(cmdline.into());
        self
    }

    /// Set the kernel release string.
    #[must_use]
    pub fn with_osrelease(mut self, release: impl Into<String>) -> Self {
        self.osrelease = Some(release.into());
        self
    }

    /// Set the Secure Boot EFI variable.
    #[must_use]
    pub fn with_secure_boot(mut self, enabled: bool) -> Self {
        self.secure_boot = Some(enabled);
        self
    }

    /// Set the firmware setup-mode flag.
    #[must_use]
    pub fn with_setup_mode(mut self, setup: bool) -> Self {
        self.setup_mode = Some(setup);
        self
    }

    /// Set the TPM major version.
    #[must_use]
    pub fn with_tpm(mut self, version: u32) -> Self {
        self.tpm_version = Some(version);
        self
    }

    /// Set whether the IOMMU hardware was found.
    #[must_use]
    pub fn with_iommu(mut self, present: bool) -> Self {
        self.iommu = Some(present);
        self
    }

    /// Set the `/sys/kernel/security/lockdown` contents.
    #[must_use]
    pub fn with_lockdown(mut self, lockdown: impl Into<String>) -> Self {
        self.lockdown = Some(lockdown.into());
        self
    }

    /// Set whether `eac-setup` / `eac-status` resolve on `PATH`.
    #[must_use]
    pub fn with_tools(mut self, eac_setup: bool, eac_status: bool) -> Self {
        self.eac_setup = eac_setup;
        self.eac_status = eac_status;
        self
    }
}

// ---------------------------------------------------------------------------
// The readiness verdict
// ---------------------------------------------------------------------------

/// What the boot chain looks like right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EacReadiness {
    /// One entry per [`Requirement::ALL`], in that order.
    pub checks: Vec<RequirementCheck>,
    /// Whether `eac-setup` is available, which is what the reboot action needs.
    pub eac_setup: bool,
    /// Whether `eac-status` is available (informational).
    pub eac_status: bool,
    /// Non-fatal read problems worth showing next to the result.
    pub warnings: Vec<String>,
}

impl EacReadiness {
    /// Whether every requirement is satisfied — the readiness verdict.
    ///
    /// This is derived from the boot reads alone: a missing `eac-status` never
    /// changes it.
    pub fn ready(&self) -> bool {
        self.checks.iter().all(|check| check.satisfied)
    }

    /// The requirements that are not satisfied, in [`Requirement::ALL`] order.
    pub fn missing(&self) -> Vec<Requirement> {
        self.checks
            .iter()
            .filter(|check| !check.satisfied)
            .map(|check| check.requirement)
            .collect()
    }

    /// The check for one requirement.
    pub fn check(&self, requirement: Requirement) -> Option<&RequirementCheck> {
        self.checks
            .iter()
            .find(|check| check.requirement == requirement)
    }

    /// Whether one requirement is satisfied (a missing entry is not).
    pub fn satisfied(&self, requirement: Requirement) -> bool {
        self.check(requirement).is_some_and(|check| check.satisfied)
    }
}

/// Read the boot chain and report EAC Linux v2 readiness.
#[must_use]
pub fn probe(input: &EacProbe) -> EacReadiness {
    let mut warnings = Vec::new();
    let tokens: Vec<&str> = input
        .cmdline
        .as_deref()
        .map(|line| line.split_whitespace().collect())
        .unwrap_or_default();
    if input.cmdline.is_none() {
        warnings.push(format!("could not read {PROC_CMDLINE}"));
    }
    if input.secure_boot.is_none() {
        warnings.push("Secure Boot state is not readable (no EFI variables)".to_string());
    }

    let mut checks = Vec::with_capacity(Requirement::ALL.len());
    for requirement in Requirement::ALL {
        checks.push(check_requirement(*requirement, input, &tokens));
    }

    EacReadiness {
        checks,
        eac_setup: input.eac_setup,
        eac_status: input.eac_status,
        warnings,
    }
}

fn check_requirement(
    requirement: Requirement,
    input: &EacProbe,
    tokens: &[&str],
) -> RequirementCheck {
    let (satisfied, detail) = match requirement {
        Requirement::SecureBoot => match input.secure_boot {
            Some(true) if input.setup_mode == Some(true) => {
                (false, "the firmware is in setup mode".to_string())
            }
            Some(true) => (true, "enabled".to_string()),
            Some(false) => (false, "disabled".to_string()),
            None => (false, "not readable".to_string()),
        },
        Requirement::Tpm2 => match input.tpm_version {
            Some(2) => (true, "TPM 2.0".to_string()),
            Some(other) => (false, format!("TPM {other}, not 2.0")),
            None => (false, "no TPM 2.0 found".to_string()),
        },
        Requirement::EacKernel => match input.osrelease.as_deref() {
            Some(release) if release.contains(EAC_KERNEL_MARKER) => {
                (true, format!("kernel {release}"))
            }
            Some(release) => (false, format!("kernel {release}")),
            None => (false, "kernel release not readable".to_string()),
        },
        Requirement::LockdownIntegrity => {
            if has_token(tokens, "lockdown=integrity") {
                (true, "lockdown=integrity".to_string())
            } else {
                (
                    false,
                    observed_lockdown(input).unwrap_or_else(|| "not in /proc/cmdline".to_string()),
                )
            }
        }
        Requirement::SignedModules => {
            if has_token(tokens, "module.sig_enforce=1") {
                (true, "module.sig_enforce=1".to_string())
            } else {
                (false, "not in /proc/cmdline".to_string())
            }
        }
        Requirement::KexecDisabled => {
            if has_token(tokens, "sysctl.kernel.kexec_load_disabled=1") {
                (true, "sysctl.kernel.kexec_load_disabled=1".to_string())
            } else {
                (false, "not in /proc/cmdline".to_string())
            }
        }
        Requirement::BpfLsm => {
            let lsm = lsm_list(tokens);
            let has_all = lsm.as_ref().is_some_and(|list| {
                list.contains("bpf") && list.contains("lockdown") && list.contains("integrity")
            });
            if has_all {
                (true, lsm.unwrap_or_default())
            } else {
                (
                    false,
                    lsm.unwrap_or_else(|| "no lsm= in /proc/cmdline".to_string()),
                )
            }
        }
        Requirement::Iommu => {
            let passthrough = has_token(tokens, "iommu.passthrough=0");
            let enabled = has_token(tokens, "intel_iommu=on") || has_token(tokens, "amd_iommu=on");
            if passthrough && enabled {
                (
                    true,
                    "iommu.passthrough=0 with an iommu=on option".to_string(),
                )
            } else if input.iommu == Some(true) {
                (
                    false,
                    "IOMMU hardware present, but the EAC cmdline options are missing".to_string(),
                )
            } else {
                (false, "not enabled".to_string())
            }
        }
    };

    RequirementCheck {
        requirement,
        satisfied,
        detail,
    }
}

fn has_token(tokens: &[&str], wanted: &str) -> bool {
    tokens.contains(&wanted)
}

/// The `lsm=` value, as a comma-separated list, when present.
fn lsm_list(tokens: &[&str]) -> Option<String> {
    tokens
        .iter()
        .find_map(|token| token.strip_prefix("lsm="))
        .map(str::to_string)
}

/// The lockdown file's active mode, when it names one.
fn observed_lockdown(input: &EacProbe) -> Option<String> {
    let text = input.lockdown.as_deref()?;
    let active = text
        .split_whitespace()
        .find(|token| token.starts_with('[') && token.ends_with(']'))
        .map(|token| token.trim_matches(['[', ']']).to_string());
    active.map(|mode| format!("lockdown is {mode}"))
}

// ---------------------------------------------------------------------------
// The three-state verdict
// ---------------------------------------------------------------------------

/// What the launcher should tell the user about an EAC-protected game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EacVerdict {
    /// The game carries no EAC: nothing to say.
    NotProtected,
    /// The game uses Easy Anti-Cheat, but not EAC Linux v2 — it cannot run on
    /// Linux yet, and rebooting would not help.
    ProtectedUnsupported { anticheat: AntiCheat },
    /// EAC Linux v2, and this boot satisfies every requirement.
    Ready,
    /// EAC Linux v2, but this is not the EAC entry. `can_reboot` is whether
    /// `eac-setup` is available to take the user there.
    NeedsEacEntry {
        missing: Vec<Requirement>,
        can_reboot: bool,
    },
}

impl EacVerdict {
    /// The short value the detail page's card shows.
    pub fn label(&self) -> String {
        match self {
            Self::NotProtected => "None".to_string(),
            Self::ProtectedUnsupported { anticheat } => {
                format!("{} — not supported on Linux", anticheat.label())
            }
            Self::Ready => "EAC Linux v2 — ready".to_string(),
            Self::NeedsEacEntry { .. } => "EAC Linux v2 — needs the EAC entry".to_string(),
        }
    }

    /// The explanation to show under the label, or `None` when there is nothing
    /// to say.
    pub fn message(&self) -> Option<String> {
        match self {
            Self::NotProtected => None,
            Self::ProtectedUnsupported { anticheat } => Some(format!(
                "{} has not enabled EAC Linux v2 for this game, so it cannot run on \
                 Linux yet. Rebooting into the EAC entry would not change that.",
                anticheat.label()
            )),
            Self::Ready => Some(
                "This boot meets every EAC Linux v2 requirement, so the game can start."
                    .to_string(),
            ),
            Self::NeedsEacEntry { missing, .. } => {
                let names = missing
                    .iter()
                    .map(|requirement| requirement.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "This game uses Easy Anti-Cheat Linux v2, which only runs from the \
                     CachyOS EAC boot entry. Missing here: {names}."
                ))
            }
        }
    }

    /// Whether the reboot action may be offered (only when it can work).
    pub const fn offers_reboot(&self) -> bool {
        matches!(
            self,
            Self::NeedsEacEntry {
                can_reboot: true,
                ..
            }
        )
    }

    /// The "new — being tested; not validated by us" note, for the states that
    /// make an EAC v2 claim.
    pub const fn notice(&self) -> Option<&'static str> {
        match self {
            Self::Ready | Self::NeedsEacEntry { .. } => Some(NOT_VALIDATED),
            Self::NotProtected | Self::ProtectedUnsupported { .. } => None,
        }
    }
}

/// Fold the curated game state with the host's boot readiness.
///
/// `anticheat` is the effective anti-cheat (detection merged with curation) and
/// `eac_v2` is the compat DB's curated claim that Epic has enabled EAC Linux v2
/// for the game. A game the launcher cannot protect is [`EacVerdict::NotProtected`];
/// an EAC game without the v2 claim stays "unsupported on Linux"; an EAC v2 game
/// is ready or needs the entry, from `readiness` alone.
#[must_use]
pub fn verdict(anticheat: AntiCheat, eac_v2: bool, readiness: &EacReadiness) -> EacVerdict {
    if eac_v2 {
        if readiness.ready() {
            return EacVerdict::Ready;
        }
        return EacVerdict::NeedsEacEntry {
            missing: readiness.missing(),
            can_reboot: readiness.eac_setup,
        };
    }

    if anticheat.is_protected() {
        return EacVerdict::ProtectedUnsupported { anticheat };
    }

    EacVerdict::NotProtected
}

// ---------------------------------------------------------------------------
// The one boot-chain action
// ---------------------------------------------------------------------------

/// The argv-only command the launcher runs for "reboot into the EAC entry and
/// play".
///
/// It is `pkexec eac-setup boot`: `pkexec` gives the action an explicit polkit
/// authentication, and `eac-setup boot` is the *only* boot-chain command the
/// launcher ever runs. The launcher never calls `sbctl`, never enrolls or signs
/// keys, never touches the TPM, and never writes an EFI variable itself — that
/// is CachyOS's `eac-setup`'s job (ADR-0031). This returns argv, never a shell
/// string, so a caller execs it directly.
#[must_use]
pub fn reboot_argv() -> Vec<String> {
    vec![
        PKEXEC.to_string(),
        EAC_SETUP.to_string(),
        "boot".to_string(),
    ]
}

/// The command to show when the reboot cannot be offered (no `eac-setup`).
#[must_use]
pub fn reboot_command_line() -> String {
    format!("sudo {EAC_SETUP} boot")
}

// ---------------------------------------------------------------------------
// Host reads
// ---------------------------------------------------------------------------

fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Read a `SecureBoot`/`SetupMode` EFI variable: 4 attribute bytes, then the
/// value. A missing variable is `None`; a variable whose value byte is absent
/// is `None` too.
fn read_efivar(dir: &Path, name: &str) -> Option<bool> {
    let entries = std::fs::read_dir(dir).ok()?;
    let prefix = format!("{name}-");
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with(&prefix) {
            continue;
        }
        let bytes = std::fs::read(entry.path()).ok()?;
        return Some(bytes.get(4).is_some_and(|value| *value != 0));
    }
    None
}

/// Whether a directory exists and holds at least one entry.
fn read_dir_nonempty(dir: &Path) -> Option<bool> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(entries.flatten().next().is_some())
}

/// Whether `program` resolves on `PATH`.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full CachyOS EAC command line, as `eac-setup` writes it.
    const EAC_CMDLINE: &str = "BOOT_IMAGE=/vmlinuz-linux-cachyos-eac root=UUID=abc rw splash quiet \
        lockdown=integrity lsm=landlock,lockdown,yama,integrity,bpf iommu.passthrough=0 \
        intel_iommu=on module.sig_enforce=1 sysctl.kernel.kexec_load_disabled=1";

    fn eac_probe() -> EacProbe {
        EacProbe::at(EAC_CMDLINE)
            .with_osrelease("7.3.3-1-cachyos-eac")
            .with_secure_boot(true)
            .with_tpm(2)
            .with_iommu(true)
            .with_tools(true, true)
    }

    #[test]
    fn a_full_eac_boot_is_ready() {
        let readiness = probe(&eac_probe());

        assert!(readiness.ready(), "missing: {:?}", readiness.missing());
        assert!(readiness.missing().is_empty());
        assert!(readiness.eac_setup);
        assert!(readiness.eac_status);
        assert!(readiness.warnings.is_empty(), "{:?}", readiness.warnings);
        for requirement in Requirement::ALL {
            assert!(
                readiness.satisfied(*requirement),
                "{requirement:?} should be satisfied"
            );
        }
    }

    #[test]
    fn a_normal_cachyos_boot_is_not_ready_and_names_every_gap() {
        // The machine's normal entry: none of the EAC options, no EAC kernel.
        let normal = EacProbe::at("root=UUID=abc rw splash quiet")
            .with_osrelease("7.3.3-1-cachyos")
            .with_secure_boot(true)
            .with_tpm(2)
            .with_tools(true, true);

        let readiness = probe(&normal);

        assert!(!readiness.ready());
        assert_eq!(
            readiness.missing(),
            vec![
                Requirement::EacKernel,
                Requirement::LockdownIntegrity,
                Requirement::SignedModules,
                Requirement::KexecDisabled,
                Requirement::BpfLsm,
                Requirement::Iommu,
            ]
        );
        // Secure Boot and the TPM are already fine on the normal entry.
        assert!(readiness.satisfied(Requirement::SecureBoot));
        assert!(readiness.satisfied(Requirement::Tpm2));
    }

    #[test]
    fn each_requirement_fails_on_its_own() {
        // Start from a ready boot and break one thing at a time.
        let cases: &[(Requirement, EacProbe)] = &[
            (Requirement::SecureBoot, eac_probe().with_secure_boot(false)),
            (
                Requirement::SecureBoot,
                eac_probe().with_secure_boot(true).with_setup_mode(true),
            ),
            (Requirement::Tpm2, eac_probe().with_tpm(1)),
            (
                Requirement::EacKernel,
                eac_probe().with_osrelease("7.3.3-1-cachyos"),
            ),
            (
                Requirement::LockdownIntegrity,
                EacProbe::at(EAC_CMDLINE.replace("lockdown=integrity", "lockdown=none"))
                    .with_osrelease("7.3.3-1-cachyos-eac")
                    .with_secure_boot(true)
                    .with_tpm(2)
                    .with_tools(true, true),
            ),
            (
                Requirement::SignedModules,
                EacProbe::at(EAC_CMDLINE.replace("module.sig_enforce=1", "module.sig_enforce=0"))
                    .with_osrelease("7.3.3-1-cachyos-eac")
                    .with_secure_boot(true)
                    .with_tpm(2)
                    .with_tools(true, true),
            ),
            (
                Requirement::KexecDisabled,
                EacProbe::at(EAC_CMDLINE.replace(
                    "sysctl.kernel.kexec_load_disabled=1",
                    "sysctl.kernel.kexec_load_disabled=0",
                ))
                .with_osrelease("7.3.3-1-cachyos-eac")
                .with_secure_boot(true)
                .with_tpm(2)
                .with_tools(true, true),
            ),
            (
                Requirement::BpfLsm,
                EacProbe::at(
                    EAC_CMDLINE.replace("landlock,lockdown,yama,integrity,bpf", "landlock,yama"),
                )
                .with_osrelease("7.3.3-1-cachyos-eac")
                .with_secure_boot(true)
                .with_tpm(2)
                .with_tools(true, true),
            ),
            (
                Requirement::Iommu,
                EacProbe::at(EAC_CMDLINE.replace("iommu.passthrough=0", "iommu.passthrough=1"))
                    .with_osrelease("7.3.3-1-cachyos-eac")
                    .with_secure_boot(true)
                    .with_tpm(2)
                    .with_tools(true, true),
            ),
        ];

        for (requirement, input) in cases {
            let readiness = probe(input);
            assert!(
                !readiness.satisfied(*requirement),
                "{requirement:?} should not be satisfied"
            );
            assert!(
                readiness.missing().contains(requirement),
                "{requirement:?} should be missing"
            );
        }
    }

    #[test]
    fn cmdline_options_are_matched_as_whole_tokens() {
        // A substring must not satisfy a requirement.
        let weird = EacProbe::at("foo=lockdown=integritybar module.sig_enforce=10")
            .with_secure_boot(true)
            .with_tpm(2)
            .with_tools(false, false);
        let readiness = probe(&weird);
        assert!(!readiness.satisfied(Requirement::LockdownIntegrity));
        assert!(!readiness.satisfied(Requirement::SignedModules));
    }

    #[test]
    fn readiness_never_depends_on_eac_status() {
        // The verification tool is absent; the boot still satisfies everything.
        let readiness = probe(&eac_probe().with_tools(true, false));
        assert!(readiness.ready());
        assert!(!readiness.eac_status);

        // Even with neither tool present the readiness answer is unchanged.
        let readiness = probe(&eac_probe().with_tools(false, false));
        assert!(readiness.ready());
        assert!(!readiness.eac_setup);
    }

    #[test]
    fn a_missing_cmdline_is_never_ready_and_warns() {
        let readiness = probe(&EacProbe::default());
        assert!(!readiness.ready());
        assert!(
            readiness
                .warnings
                .iter()
                .any(|line| line.contains(PROC_CMDLINE)),
            "{:?}",
            readiness.warnings
        );
        assert!(!readiness.satisfied(Requirement::EacKernel));
    }

    #[test]
    fn the_verdict_distinguishes_the_three_states() {
        let ready = probe(&eac_probe());
        let off_entry = probe(
            &EacProbe::at("root=UUID=abc")
                .with_secure_boot(true)
                .with_tpm(2)
                .with_tools(true, true),
        );

        // Protected but not EAC v2: unsupported on Linux, no reboot offered.
        let unsupported = verdict(AntiCheat::Eac, false, &ready);
        assert_eq!(
            unsupported,
            EacVerdict::ProtectedUnsupported {
                anticheat: AntiCheat::Eac
            }
        );
        assert!(!unsupported.offers_reboot());
        assert!(unsupported.notice().is_none());
        assert!(unsupported.message().unwrap().contains("not enabled"));

        // EAC v2 on the entry: ready.
        let on_entry = verdict(AntiCheat::Eac, true, &ready);
        assert_eq!(on_entry, EacVerdict::Ready);
        assert!(on_entry.message().unwrap().contains("can start"));
        assert_eq!(on_entry.notice(), Some(NOT_VALIDATED));

        // EAC v2 off the entry: needs the EAC entry, offers the reboot.
        let needs = verdict(AntiCheat::Eac, true, &off_entry);
        let EacVerdict::NeedsEacEntry {
            missing,
            can_reboot,
        } = &needs
        else {
            panic!("expected NeedsEacEntry, got {needs:?}");
        };
        assert!(can_reboot);
        assert!(missing.contains(&Requirement::LockdownIntegrity));
        assert!(needs.offers_reboot());
        assert!(needs.message().unwrap().contains("EAC boot entry"));
        assert_eq!(needs.notice(), Some(NOT_VALIDATED));

        // No anti-cheat at all: nothing to say.
        assert_eq!(
            verdict(AntiCheat::None, false, &ready),
            EacVerdict::NotProtected
        );
        assert_eq!(
            verdict(AntiCheat::Unknown, false, &ready),
            EacVerdict::NotProtected
        );
    }

    #[test]
    fn eac_v2_without_eac_setup_cannot_offer_the_reboot() {
        let off_entry = probe(
            &EacProbe::at("root=UUID=abc")
                .with_secure_boot(true)
                .with_tpm(2)
                .with_tools(false, false),
        );
        let verdict = verdict(AntiCheat::Eac, true, &off_entry);
        assert!(!verdict.offers_reboot());
        assert!(!verdict.label().is_empty());
    }

    #[test]
    fn an_eac_v2_game_is_still_protected_so_injection_stays_gated() {
        // The EAC entry makes the game *runnable*; it never makes injection
        // safe. An EAC v2 game is as protected as any other, so the anti-cheat
        // gate still refuses injection without an explicit acknowledgement —
        // even on the EAC entry (ADR-0025/ADR-0031).
        let ready = probe(&eac_probe());
        assert_eq!(verdict(AntiCheat::Eac, true, &ready), EacVerdict::Ready);
        assert!(AntiCheat::Eac.is_protected());
        assert_eq!(
            crate::anticheat::gate(AntiCheat::Eac, true, false),
            crate::anticheat::GateDecision::ConfirmationRequired {
                anticheat: AntiCheat::Eac
            }
        );
    }

    #[test]
    fn a_game_with_no_curated_v2_and_no_eac_is_not_protected() {
        let ready = probe(&eac_probe());
        assert_eq!(
            verdict(AntiCheat::None, false, &ready),
            EacVerdict::NotProtected
        );
        assert!(EacVerdict::NotProtected.message().is_none());
    }

    #[test]
    fn a_fixture_tree_mirrors_slash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
        std::fs::write(root.join("proc/cmdline"), EAC_CMDLINE).unwrap();
        std::fs::write(
            root.join("proc/sys/kernel/osrelease"),
            "7.3.3-1-cachyos-eac\n",
        )
        .unwrap();

        let efivars = root.join("sys/firmware/efi/efivars");
        std::fs::create_dir_all(&efivars).unwrap();
        // An EFI variable is four attribute bytes followed by the value.
        std::fs::write(
            efivars.join("SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c"),
            [0u8, 0, 0, 0, 1],
        )
        .unwrap();
        std::fs::write(
            efivars.join("SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c"),
            [0u8, 0, 0, 0, 0],
        )
        .unwrap();

        std::fs::create_dir_all(root.join("sys/class/tpm/tpm0")).unwrap();
        std::fs::write(root.join("sys/class/tpm/tpm0/tpm_version_major"), "2\n").unwrap();
        std::fs::create_dir_all(root.join("sys/class/iommu")).unwrap();
        std::fs::write(root.join("sys/class/iommu/0"), "").unwrap();

        let readiness = probe(&EacProbe::from_root(root));
        assert!(readiness.ready(), "missing: {:?}", readiness.missing());
        // A fixture run reports the tools absent; readiness does not care.
        assert!(!readiness.eac_setup);
        assert!(!readiness.eac_status);
    }

    #[test]
    fn the_reboot_action_is_only_eac_setup_boot() {
        let argv = reboot_argv();
        assert_eq!(argv, vec!["pkexec", "eac-setup", "boot"]);
        assert_eq!(argv.last().map(String::as_str), Some("boot"));
        assert!(argv.iter().any(|arg| arg == EAC_SETUP));

        // The launcher must never touch Secure Boot keys, sbctl, the TPM or
        // signing, and never write an EFI variable itself (ADR-0031).
        for forbidden in [
            "sbctl",
            "sign",
            "key",
            "enroll",
            "tpm",
            "efi-updatevar",
            "bootnext",
            "--bootnext",
            "cachyos-eac-ca",
        ] {
            assert!(
                !argv
                    .iter()
                    .any(|arg| arg.to_lowercase().contains(forbidden)),
                "reboot argv mentions `{forbidden}`: {argv:?}"
            );
        }
        assert_eq!(reboot_command_line(), "sudo eac-setup boot");
    }

    #[test]
    fn readiness_and_verdict_round_trip_through_json_for_the_cli() {
        let readiness = probe(&eac_probe());
        let json = serde_json::to_string(&readiness).unwrap();
        assert!(json.contains(r#""requirement":"secure_boot""#), "{json}");
        assert!(json.contains(r#""satisfied":true"#), "{json}");

        let verdict = verdict(AntiCheat::Eac, true, &readiness);
        let json = serde_json::to_string(&verdict).unwrap();
        assert!(json.contains(r#""state":"ready""#), "{json}");
        let parsed: EacVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, verdict);
    }

    #[test]
    fn the_docs_table_quotes_every_requirement() {
        // The UI labels and the docs table share these strings; a test reads
        // `docs/eac-v2.md` and asserts each appears verbatim, so the two cannot
        // drift (the pattern `platform::capability` uses).
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs/eac-v2.md");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));

        for requirement in Requirement::ALL {
            assert!(
                text.contains(requirement.label()),
                "docs/eac-v2.md is missing the requirement label `{}`",
                requirement.label()
            );
            assert!(
                text.contains(requirement.as_str()),
                "docs/eac-v2.md is missing the requirement id `{}`",
                requirement.as_str()
            );
        }
        assert!(
            text.contains(NOT_VALIDATED),
            "docs/eac-v2.md must carry the `{NOT_VALIDATED}` marker"
        );
    }
}
