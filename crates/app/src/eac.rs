//! EAC Linux v2 summary for the game detail page.
//!
//! The hard rules live in `platform::eac`; this module only turns the game's
//! curated `eac_v2` claim and the host's boot readiness into the card, the
//! explanation and the consent-gated reboot the detail page shows. It is
//! framework-free so it can be unit-tested without a webview, like
//! `anticheat.rs` and `runtime.rs`.

use pitagames_core::AntiCheat;
use pitagames_platform::eac::{self, EacReadiness, EacVerdict};

/// What the detail page renders for one game's EAC Linux v2 state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EacView {
    /// Whether the card is shown at all (a game with no EAC shows nothing).
    pub show: bool,
    /// The short value, e.g. `EAC Linux v2 — needs the EAC entry`.
    pub label: String,
    /// The explanation under the label.
    pub message: Option<String>,
    /// The "new — being tested; not validated by us" note.
    pub notice: Option<String>,
    /// The missing requirements, `label — fix`, when the entry is needed.
    pub missing: Vec<String>,
    /// The argv to run on confirmation, present only when the reboot may be
    /// offered (the game needs the entry *and* `eac-setup` is installed).
    pub reboot: Option<Vec<String>>,
    /// Whether the game needs the EAC entry (so the command line is worth
    /// showing when the reboot cannot be offered).
    pub needs_entry: bool,
    /// The command to show when the reboot cannot be offered.
    pub command_line: String,
}

impl EacView {
    /// The placeholder shown until the probe resolves (nothing is shown).
    pub fn hidden() -> Self {
        Self {
            show: false,
            label: String::new(),
            message: None,
            notice: None,
            missing: Vec::new(),
            reboot: None,
            needs_entry: false,
            command_line: eac::reboot_command_line(),
        }
    }
}

/// Fold the curated game state and the host's readiness into the page's view.
#[must_use]
pub fn summarize(anticheat: AntiCheat, eac_v2: bool, readiness: &EacReadiness) -> EacView {
    let verdict = eac::verdict(anticheat, eac_v2, readiness);

    let missing = match &verdict {
        EacVerdict::NeedsEacEntry { missing, .. } => missing
            .iter()
            .map(|requirement| format!("{} — {}", requirement.label(), requirement.fix()))
            .collect(),
        _ => Vec::new(),
    };

    EacView {
        show: !matches!(verdict, EacVerdict::NotProtected),
        label: verdict.label(),
        message: verdict.message(),
        notice: verdict.notice().map(str::to_string),
        missing,
        reboot: verdict.offers_reboot().then(eac::reboot_argv),
        needs_entry: matches!(verdict, EacVerdict::NeedsEacEntry { .. }),
        command_line: eac::reboot_command_line(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_platform::eac::{EacProbe, Requirement};

    const EAC_CMDLINE: &str = "lockdown=integrity lsm=landlock,lockdown,yama,integrity,bpf \
        iommu.passthrough=0 intel_iommu=on module.sig_enforce=1 \
        sysctl.kernel.kexec_load_disabled=1";

    fn ready_probe() -> EacReadiness {
        eac::probe(
            &EacProbe::at(EAC_CMDLINE)
                .with_osrelease("7.3.3-1-cachyos-eac")
                .with_secure_boot(true)
                .with_tpm(2)
                .with_tools(true, true),
        )
    }

    fn off_entry_probe() -> EacReadiness {
        eac::probe(
            &EacProbe::at("root=UUID=abc")
                .with_secure_boot(true)
                .with_tpm(2)
                .with_tools(true, true),
        )
    }

    #[test]
    fn a_game_with_no_eac_shows_nothing() {
        let view = summarize(AntiCheat::None, false, &ready_probe());
        assert!(!view.show);
        assert!(view.message.is_none());
        assert!(view.reboot.is_none());
    }

    #[test]
    fn a_protected_game_without_v2_explains_it_cannot_run_on_linux() {
        let view = summarize(AntiCheat::Eac, false, &ready_probe());
        assert!(view.show);
        assert!(
            view.label.contains("not supported on Linux"),
            "{}",
            view.label
        );
        assert!(view.message.unwrap().contains("not enabled"));
        assert!(view.reboot.is_none());
        assert!(view.notice.is_none());
    }

    #[test]
    fn an_eac_v2_game_on_the_entry_confirms_readiness() {
        let view = summarize(AntiCheat::Eac, true, &ready_probe());
        assert!(view.show);
        assert!(view.label.contains("ready"), "{}", view.label);
        assert!(view.message.unwrap().contains("can start"));
        assert!(view.reboot.is_none());
        assert_eq!(view.notice.as_deref(), Some(eac::NOT_VALIDATED));
    }

    #[test]
    fn an_eac_v2_game_off_the_entry_offers_the_reboot() {
        let view = summarize(AntiCheat::Eac, true, &off_entry_probe());
        assert!(view.show);
        assert!(view.label.contains("needs the EAC entry"), "{}", view.label);
        assert!(view.missing.iter().any(|line| line.contains("EAC kernel")));
        assert!(
            view.missing
                .iter()
                .any(|line| line.contains(Requirement::LockdownIntegrity.fix()))
        );
        assert_eq!(view.reboot, Some(eac::reboot_argv()));
        assert!(view.needs_entry);
        assert_eq!(view.command_line, "sudo eac-setup boot");
        assert_eq!(view.notice.as_deref(), Some(eac::NOT_VALIDATED));
    }

    #[test]
    fn off_the_entry_without_eac_setup_shows_the_command_instead() {
        let readiness = eac::probe(
            &EacProbe::at("root=UUID=abc")
                .with_secure_boot(true)
                .with_tpm(2)
                .with_tools(false, false),
        );
        let view = summarize(AntiCheat::Eac, true, &readiness);
        assert!(view.reboot.is_none());
        assert_eq!(view.command_line, "sudo eac-setup boot");
    }
}
