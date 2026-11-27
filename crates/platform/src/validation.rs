//! The graphics-validation record: one path per row, one machine per column.
//!
//! `docs/graphics-validation.md` is the human-readable certification record for
//! the plan §9 hardware-validation matrix on our two in-house machines
//! (ADR-0004). This module is the record the document and the tests read from:
//! [`ValidationPath::claim`] answers each cell, and a machine's own claim is
//! derived from the shipped compat DB's per-game `verified` field
//! ([`Machine::db_claim`]). The document's claims and that machine-readable
//! field therefore cannot drift — a test reads the document and checks every row
//! against this module, and checks the module against the real `compat/`
//! database.
//!
//! Two rules, straight from the plan and ADR-0004:
//!
//! - The only architecture the shipped compat DB verifies is **RDNA 4**, so the
//!   RX 9070 column is `verified` and every path on the RDNA 1 card (or on any
//!   other hardware) is `experimental` — the UI must say "not tested by us"
//!   rather than imply parity.
//! - A path the hardware cannot run is `not supported`, one that does not apply
//!   is `n/a`, and the TV paths are `unvalidated` (no TV in-house).
//!
//! The module is framework-free and does no I/O of its own: the compat database
//! is passed in, so the record is a pure function of the shipped data.

use crate::compat::{Arch, CompatDb};

/// A machine the project can validate in-house (ADR-0004).
///
/// Exactly two, both in-house; everything else ships `experimental`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Machine {
    /// RX 9070 — RDNA 4, 16 GB. The validated primary card.
    Rx9070,
    /// RX 5700 XT — RDNA 1, 8 GB. The in-house secondary card.
    Rx5700Xt,
}

impl Machine {
    /// Both in-house machines, in the order the docs table presents them.
    pub const ALL: [Machine; 2] = [Machine::Rx9070, Machine::Rx5700Xt];

    /// The label the UI and the docs table show.
    pub const fn label(self) -> &'static str {
        match self {
            Machine::Rx9070 => "RX 9070 (RDNA 4)",
            Machine::Rx5700Xt => "RX 5700 XT (RDNA 1)",
        }
    }

    /// A short form for a table header.
    pub const fn short(self) -> &'static str {
        match self {
            Machine::Rx9070 => "RX 9070",
            Machine::Rx5700Xt => "RX 5700 XT",
        }
    }

    /// The architecture this machine detects as.
    pub const fn arch(self) -> Arch {
        match self {
            Machine::Rx9070 => Arch::Rdna4,
            Machine::Rx5700Xt => Arch::Rdna1,
        }
    }

    /// The claim the shipped compat DB backs for this machine.
    ///
    /// `Verified` exactly when the DB lists this machine's architecture in a
    /// per-game `verified` field — the machine-readable claim. Today only
    /// `rdna4` is listed, so the RX 5700 XT is `Experimental` ("not tested by
    /// us") until its hardware pass.
    pub fn db_claim(self, db: &CompatDb) -> Claim {
        if db.verified_for(self.arch()).next().is_some() {
            Claim::Verified
        } else {
            Claim::Experimental
        }
    }
}

/// The validation claim for one path on one machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Claim {
    /// Verified on this machine — the compat DB lists its architecture.
    Verified,
    /// Offered with no in-house coverage: "not tested by us".
    Experimental,
    /// The hardware cannot run this path.
    NotSupported,
    /// The path does not apply to this machine.
    NotApplicable,
    /// Not testable in-house (no TV): "not tested by us".
    Unvalidated,
}

impl Claim {
    /// The bare claim word, for a machine-level summary line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Claim::Verified => "verified",
            Claim::Experimental => "experimental",
            Claim::NotSupported => "not supported",
            Claim::NotApplicable => "n/a",
            Claim::Unvalidated => "unvalidated",
        }
    }

    /// The cell text the docs table shows.
    ///
    /// The two claims with no in-house coverage carry the "not tested by us"
    /// marker inline, so a reader (and the docs test) sees it on every untested
    /// cell.
    pub const fn cell(self) -> &'static str {
        match self {
            Claim::Verified => "verified",
            Claim::Experimental => "experimental (not tested by us)",
            Claim::NotSupported => "not supported",
            Claim::NotApplicable => "n/a",
            Claim::Unvalidated => "unvalidated (not tested by us)",
        }
    }

    /// Whether the claim is backed by hardware the project validated.
    pub const fn is_verified(self) -> bool {
        matches!(self, Claim::Verified)
    }

    /// Whether the claim carries the "not tested by us" marker.
    pub const fn is_untested(self) -> bool {
        matches!(self, Claim::Experimental | Claim::Unvalidated)
    }
}

/// One row of the plan §9 hardware-validation matrix.
///
/// [`ValidationPath::ALL`] is the one order `docs/graphics-validation.md` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValidationPath {
    /// Store launch through the shim: Steam / EGS / GOG / Amazon.
    StoreLaunch,
    /// Proton-CachyOS's in-place `PROTON_FSR4_UPGRADE`.
    Fsr4Upgrade,
    /// OptiScaler presenting FSR 4 to a DLSS game (Tier A + Tier B).
    DlssToFsr4Optiscaler,
    /// The FSR 4 → XeSS / FSR 3.1 fallback routes.
    Fsr4Fallbacks,
    /// MLFG and OptiScaler FSR 3 frame generation.
    FrameGeneration,
    /// DLSS 5 neural rendering through DLSSNR-AMD upstream.
    NeuralRendering,
    /// HelixSR Scaler (HLX DLSS / HLX FSR 4).
    HelixSrScaler,
    /// The HelixSR FSR 3.1 drop-in.
    HelixSrDropIn,
    /// gamescope's system FSR.
    SystemFsr,
    /// HDR / VRR / gamescope.
    HdrVrrGamescope,
    /// The in-app console UI, windowed → fullscreen.
    ConsoleUi,
    /// Console 10-foot readability / safe area (no TV in-house).
    ConsoleTenFoot,
    /// The bootable gamescope session (v1.1, no TV in-house).
    BootableGamescopeSession,
    /// Controller navigation (Xbox / DualSense / 8BitDo).
    ControllerNavigation,
    /// The architectures we do not own (RDNA 3 NR fork, RDNA 2, NVIDIA, Intel).
    OtherHardware,
}

impl ValidationPath {
    /// Every path, in the order the docs table presents them.
    pub const ALL: [ValidationPath; 15] = [
        ValidationPath::StoreLaunch,
        ValidationPath::Fsr4Upgrade,
        ValidationPath::DlssToFsr4Optiscaler,
        ValidationPath::Fsr4Fallbacks,
        ValidationPath::FrameGeneration,
        ValidationPath::NeuralRendering,
        ValidationPath::HelixSrScaler,
        ValidationPath::HelixSrDropIn,
        ValidationPath::SystemFsr,
        ValidationPath::HdrVrrGamescope,
        ValidationPath::ConsoleUi,
        ValidationPath::ConsoleTenFoot,
        ValidationPath::BootableGamescopeSession,
        ValidationPath::ControllerNavigation,
        ValidationPath::OtherHardware,
    ];

    /// The label the docs table and the CLI show.
    pub const fn label(self) -> &'static str {
        match self {
            ValidationPath::StoreLaunch => "Steam / EGS / GOG / Amazon launch + shim env",
            ValidationPath::Fsr4Upgrade => "`PROTON_FSR4_UPGRADE` (in-place FSR 4)",
            ValidationPath::DlssToFsr4Optiscaler => "DLSS to FSR 4 via OptiScaler (Tier A + B)",
            ValidationPath::Fsr4Fallbacks => "FSR 4 to XeSS / FSR 3.1 fallbacks",
            ValidationPath::FrameGeneration => "Frame generation (MLFG / FSR 3 FG)",
            ValidationPath::NeuralRendering => "DLSS 5 neural rendering (DLSSNR-AMD upstream)",
            ValidationPath::HelixSrScaler => "HelixSR Scaler (HLX DLSS / HLX FSR 4)",
            ValidationPath::HelixSrDropIn => "HelixSR FSR 3.1 drop-in",
            ValidationPath::SystemFsr => "System FSR (gamescope)",
            ValidationPath::HdrVrrGamescope => "HDR / VRR / gamescope",
            ValidationPath::ConsoleUi => "Console UI (in-app, windowed to fullscreen)",
            ValidationPath::ConsoleTenFoot => "Console 10-foot readability / safe area",
            ValidationPath::BootableGamescopeSession => "Bootable gamescope session (v1.1)",
            ValidationPath::ControllerNavigation => {
                "Controller navigation (Xbox / DualSense / 8BitDo)"
            }
            ValidationPath::OtherHardware => "RDNA 3 NR fork, RDNA 2, NVIDIA, Intel",
        }
    }

    /// The claim for this path on this machine.
    ///
    /// A path the hardware cannot run or that does not apply is fixed by the
    /// plan; the TV paths are `unvalidated`; the other architectures are
    /// `experimental`; everything the plan targets for certification follows the
    /// machine's own claim from the compat DB.
    pub fn claim(self, machine: Machine, db: &CompatDb) -> Claim {
        match self {
            // FSR 4 and its OptiScaler routes need an RDNA 3+ card.
            ValidationPath::Fsr4Upgrade | ValidationPath::DlssToFsr4Optiscaler
                if machine == Machine::Rx5700Xt =>
            {
                Claim::NotApplicable
            }
            // Neural rendering needs an RDNA 3/4 card; RDNA 1 is not supported.
            ValidationPath::NeuralRendering if machine == Machine::Rx5700Xt => Claim::NotSupported,
            // HelixSR ships experimental on both in-house cards: the RX 5700 XT
            // smoke test (gfx1010 vs upstream's gfx1013) has not run yet.
            ValidationPath::HelixSrScaler | ValidationPath::HelixSrDropIn => Claim::Experimental,
            // No TV is attached to either machine (ADR-0004).
            ValidationPath::ConsoleTenFoot | ValidationPath::BootableGamescopeSession => {
                Claim::Unvalidated
            }
            // Hardware we do not own is community-only.
            ValidationPath::OtherHardware => Claim::Experimental,
            // Everything else follows the machine's own claim: verified on the
            // RX 9070 (the DB's `verified = ["rdna4"]`), experimental elsewhere.
            _ => machine.db_claim(db),
        }
    }

    /// The markdown table row `docs/graphics-validation.md` must contain.
    ///
    /// One source for the document and the docs-agreement test: the test builds
    /// this exact string and asserts the file carries it.
    pub fn table_row(self, db: &CompatDb) -> String {
        let cells: Vec<&str> = Machine::ALL
            .iter()
            .map(|machine| self.claim(*machine, db).cell())
            .collect();
        format!("| {} | {} |", self.label(), cells.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The repository's real shipped compat database.
    fn shipped_db() -> CompatDb {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("compat");
        CompatDb::load_from(&[dir], &[])
    }

    fn doc() -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs/graphics-validation.md");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
    }

    #[test]
    fn both_machines_are_in_house_and_map_to_their_arch() {
        assert_eq!(Machine::Rx9070.arch(), Arch::Rdna4);
        assert_eq!(Machine::Rx5700Xt.arch(), Arch::Rdna1);
        for machine in Machine::ALL {
            assert!(
                machine.arch().is_in_house(),
                "{} must be an in-house card",
                machine.label()
            );
        }
    }

    #[test]
    fn the_machine_claim_follows_the_compat_verified_field() {
        let db = shipped_db();
        // The shipped DB verifies RDNA 4 and nothing else.
        assert!(db.verified_for(Arch::Rdna4).next().is_some());
        assert_eq!(Machine::Rx9070.db_claim(&db), Claim::Verified);
        assert_eq!(Machine::Rx5700Xt.db_claim(&db), Claim::Experimental);
    }

    #[test]
    fn non_rdna4_paths_stay_experimental_in_the_compat_db() {
        // The "experimental marks" the ticket asks for: no compat entry lists a
        // non-RDNA4 architecture in its `verified` field.
        let db = shipped_db();
        for arch in [Arch::Rdna3, Arch::Rdna2, Arch::Rdna1, Arch::Rtx, Arch::Arc] {
            assert_eq!(
                db.verified_for(arch).count(),
                0,
                "{} must stay experimental in compat/",
                arch.as_str()
            );
        }
    }

    #[test]
    fn the_rx5700xt_column_is_never_verified() {
        let db = shipped_db();
        for path in ValidationPath::ALL {
            assert_ne!(
                path.claim(Machine::Rx5700Xt, &db),
                Claim::Verified,
                "{} is a non-RDNA4 path and must be experimental",
                path.label()
            );
        }
    }

    #[test]
    fn every_path_has_a_row_in_the_document() {
        let db = shipped_db();
        let doc = doc();
        for path in ValidationPath::ALL {
            assert!(
                doc.contains(path.label()),
                "docs/graphics-validation.md is missing the path `{}`",
                path.label()
            );
            let row = path.table_row(&db);
            assert!(
                doc.contains(&row),
                "docs/graphics-validation.md is missing the row `{row}`"
            );
        }
    }

    #[test]
    fn the_doc_claim_traces_to_the_compat_verified_field() {
        let db = shipped_db();
        let doc = doc();

        // The machine summary line is the DB-backed claim, verbatim.
        for machine in Machine::ALL {
            let line = format!("{} - {}", machine.label(), machine.db_claim(&db).as_str());
            assert!(
                doc.contains(&line),
                "docs/graphics-validation.md is missing the machine line `{line}`"
            );
        }

        // No cell is `verified` unless the DB verifies that machine's arch — the
        // machine-readable tie between the document and the `verified` field.
        for path in ValidationPath::ALL {
            for machine in Machine::ALL {
                if path.claim(machine, &db).is_verified() {
                    assert!(
                        db.verified_for(machine.arch()).next().is_some(),
                        "{} claims verified for {} but the DB verifies no entry for {}",
                        path.label(),
                        machine.label(),
                        machine.arch().as_str()
                    );
                }
            }
        }

        // Anything untested carries the marker.
        assert!(
            doc.contains("not tested by us"),
            "docs/graphics-validation.md must mark untested paths"
        );
    }

    #[test]
    fn the_claim_texts_are_distinct_and_untested_ones_carry_the_marker() {
        assert_eq!(Claim::Verified.as_str(), "verified");
        assert_eq!(Claim::NotApplicable.cell(), "n/a");
        assert_eq!(Claim::NotSupported.cell(), "not supported");
        assert!(Claim::Experimental.is_untested());
        assert!(Claim::Unvalidated.is_untested());
        assert!(!Claim::Verified.is_untested());
        assert!(Claim::Experimental.cell().contains("not tested by us"));
        assert!(Claim::Unvalidated.cell().contains("not tested by us"));
    }
}
