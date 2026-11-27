//! The Graphics screen's model: sections, availability, and the edit stack.
//!
//! Framework-free and unit-tested, like `grid.rs` and `runtime.rs`: it turns the
//! capability matrix ([`pitagames_platform::capability`]) into the rows the
//! screen draws, and it owns the three things a user does to a profile —
//! **apply**, **revert** and **undo** — plus **reset to safe defaults**.
//!
//! Two rules shape the whole module:
//!
//! - **Only available options are offered, and the rest say why.** A row is
//!   drawn when the capability matrix says [`Availability::Available`]; a row it
//!   hides is not a greyed button but an absent one, with the matrix's own
//!   reason string beside the section ([`hidden`]). The *current* selection is
//!   always drawn, so a profile can never show a state the screen cannot
//!   represent.
//! - **One injection slot.** The single slot is not re-implemented here: the
//!   matrix already hides DLSSNR while an injected upscaler owns the slot and
//!   vice versa (ADR-0023), so switching one choice hides the other through the
//!   same `availability()` call the compiler consumes.
//!
//! The durable revert is *not* in this module: the profile's in-memory undo
//! stack is a session convenience, while the files a Tier B injection drops into
//! a game directory are reverted from the mod manifest
//! ([`durable_revert`] + `mods::manifest::ModManifest::restore`).

use std::path::Path;

use pitagames_core::AntiCheat;
use pitagames_core::profile::{FrameGen, Injection, Profile, Upscaler};
use pitagames_platform::anticheat::{self, GateDecision};
use pitagames_platform::capability::{self, Availability, Field};
use pitagames_platform::compat::CompatEntry;
use pitagames_platform::gpu::GpuProfile;
use pitagames_platform::proton::ProtonInstall;

use crate::runtime::{DiagnosticToggle, DiagnosticsDraft};

/// One section of the graphics screen, in the order the screen reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Upscaling,
    FrameGen,
    Injection,
    Diagnostics,
}

impl Section {
    /// Every section, in order.
    pub const ALL: [Section; 4] = [
        Section::Upscaling,
        Section::FrameGen,
        Section::Injection,
        Section::Diagnostics,
    ];

    /// The heading the section draws.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Upscaling => "Upscaling",
            Self::FrameGen => "Frame generation",
            Self::Injection => "Injection",
            Self::Diagnostics => "Diagnostics",
        }
    }

    /// The one-line note under the heading.
    pub const fn hint(self) -> &'static str {
        match self {
            Self::Upscaling => "How the game's image is scaled up.",
            Self::FrameGen => "Insert generated frames between real ones.",
            Self::Injection => {
                "One slot: Proton's env hooks, managed OptiScaler, HelixSR or DLSSNR, never two."
            }
            Self::Diagnostics => "Overlays and logs, for telling whether a profile worked.",
        }
    }
}

/// One thing the screen offers the user to press.
///
/// A `Choice` is the closed set of profile edits the screen makes: an upscaler,
/// a frame-gen mode, an injection owner, or a diagnostics toggle. It knows the
/// capability [`Field`] it asks about, the section it belongs to, whether the
/// profile already has it, and how to write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Upscaler(Upscaler),
    FrameGen(FrameGen),
    Injection(Injection),
    Diagnostic(DiagnosticToggle),
}

impl Choice {
    /// The section this choice is drawn in.
    pub const fn section(&self) -> Section {
        match self {
            Self::Upscaler(_) => Section::Upscaling,
            Self::FrameGen(_) => Section::FrameGen,
            Self::Injection(_) => Section::Injection,
            Self::Diagnostic(_) => Section::Diagnostics,
        }
    }

    /// The capability field this choice asks about.
    ///
    /// `None` for a choice the matrix always offers (diagnostics, and the
    /// upscaler/frame-gen/injection "off" or "automatic" rows): those depend on
    /// no detected hardware, so they are never hidden.
    pub fn field(&self) -> Option<Field> {
        match self {
            Self::Upscaler(upscaler) => Field::for_upscaler(upscaler),
            Self::FrameGen(frame_gen) => Field::for_frame_gen(*frame_gen),
            Self::Injection(injection) => Field::for_injection(*injection),
            Self::Diagnostic(_) => None,
        }
    }

    /// The label the row wears.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Upscaler(upscaler) => upscaler_label(upscaler),
            Self::FrameGen(frame_gen) => frame_gen_label(*frame_gen),
            Self::Injection(injection) => injection_label(*injection),
            Self::Diagnostic(toggle) => toggle.label(),
        }
    }

    /// Whether the profile already has this choice.
    pub fn is_selected(&self, profile: &Profile) -> bool {
        match self {
            Self::Upscaler(upscaler) => &profile.upscaler == upscaler,
            Self::FrameGen(frame_gen) => profile.frame_gen == *frame_gen,
            Self::Injection(injection) => profile.injection == *injection,
            Self::Diagnostic(toggle) => {
                DiagnosticsDraft::from_diagnostics(&profile.diagnostics).is_on(*toggle)
            }
        }
    }

    /// Write this choice into a profile.
    ///
    /// A selection replaces the field; a diagnostics toggle flips its bit (so a
    /// second press turns it off), matching the editor's own toggles.
    pub fn press(&self, profile: &mut Profile) {
        match self {
            Self::Upscaler(upscaler) => profile.upscaler = upscaler.clone(),
            Self::FrameGen(frame_gen) => profile.frame_gen = *frame_gen,
            Self::Injection(injection) => profile.injection = *injection,
            Self::Diagnostic(toggle) => {
                let mut draft = DiagnosticsDraft::from_diagnostics(&profile.diagnostics);
                draft.toggle(*toggle);
                profile.diagnostics = draft.to_diagnostics();
            }
        }
    }
}

const fn upscaler_label(upscaler: &Upscaler) -> &'static str {
    match upscaler {
        Upscaler::Off => "Off (the game's own setting)",
        Upscaler::Native => "Native (no injection)",
        Upscaler::SystemFsr => "System FSR (gamescope)",
        Upscaler::Fsr4Upgrade { .. } => "FSR 4 upgrade",
        Upscaler::Fsr4Rdna3 { .. } => "FSR 4 RDNA 3 upgrade (experimental)",
        Upscaler::DlssToFsr4 => "DLSS to FSR 4 (OptiScaler)",
        Upscaler::DlssToFsr31 => "DLSS to FSR 3.1 (OptiScaler)",
        Upscaler::DlssToXess => "DLSS to XeSS (OptiScaler)",
        Upscaler::XessUpgrade => "XeSS upgrade",
        Upscaler::Ffx3Upgrade => "FFX 3 upgrade",
        Upscaler::Ffx4Upgrade => "FFX 4 upgrade",
        Upscaler::HlxDlss => "HelixSR HLX DLSS",
        Upscaler::HlxFsr4 => "HelixSR HLX FSR 4",
        Upscaler::HlxFsr4DropIn => "HelixSR FSR 4 drop-in",
    }
}

const fn frame_gen_label(frame_gen: FrameGen) -> &'static str {
    match frame_gen {
        FrameGen::Off => "Off",
        FrameGen::Fsr4Mlfg => "ML frame generation (FSR 4)",
        FrameGen::Fsr3 => "FSR 3 frame generation (OptiScaler)",
    }
}

const fn injection_label(injection: Injection) -> &'static str {
    match injection {
        Injection::Auto => "Automatic",
        Injection::EnvOnly => "Tier A (Proton env-only)",
        Injection::Manual => "Tier B (managed OptiScaler)",
        Injection::HelixSr => "HelixSR (its own OptiScaler)",
        Injection::DlssNr => "DLSSNR (its own OptiScaler)",
        Injection::Off => "Off (no injection)",
    }
}

/// Every choice the screen can draw, in section order.
///
/// This is the one list the rows are built from; a choice whose field the matrix
/// hides is dropped from the visible rows (unless it is the current selection).
pub fn choices() -> Vec<Choice> {
    let mut out = Vec::new();
    for upscaler in [
        Upscaler::Off,
        Upscaler::Native,
        Upscaler::SystemFsr,
        Upscaler::Fsr4Upgrade { version: None },
        Upscaler::Fsr4Rdna3 { version: None },
        Upscaler::DlssToFsr4,
        Upscaler::DlssToFsr31,
        Upscaler::DlssToXess,
        Upscaler::XessUpgrade,
        Upscaler::Ffx3Upgrade,
        Upscaler::Ffx4Upgrade,
        Upscaler::HlxDlss,
        Upscaler::HlxFsr4,
        Upscaler::HlxFsr4DropIn,
    ] {
        out.push(Choice::Upscaler(upscaler));
    }
    for frame_gen in [FrameGen::Off, FrameGen::Fsr4Mlfg, FrameGen::Fsr3] {
        out.push(Choice::FrameGen(frame_gen));
    }
    for injection in [
        Injection::Auto,
        Injection::EnvOnly,
        Injection::Manual,
        Injection::HelixSr,
        Injection::DlssNr,
        Injection::Off,
    ] {
        out.push(Choice::Injection(injection));
    }
    for toggle in DiagnosticToggle::ALL {
        out.push(Choice::Diagnostic(toggle));
    }
    out
}

/// One row the screen draws: an available choice, or the current selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    pub choice: Choice,
    /// The matrix's verdict. A row that is drawn but `Hidden` is the current
    /// selection on a machine that no longer offers it; the reason is shown.
    pub availability: Availability,
    pub selected: bool,
}

impl OptionRow {
    /// Whether the row may be pressed (a hidden current selection may not).
    pub fn is_available(&self) -> bool {
        self.availability.is_available()
    }

    /// The reason the row is hidden, when it is.
    pub fn reason(&self) -> Option<&str> {
        self.availability.reason()
    }
}

/// One option the matrix hides, with its reason, for the section's "why not".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenRow {
    pub section: Section,
    pub label: &'static str,
    pub reason: String,
}

/// The rows the screen draws: the available choices, plus the current selection
/// even when the matrix would hide it (so the profile is always representable).
pub fn rows(
    profile: &Profile,
    gpu: &GpuProfile,
    compat: Option<&CompatEntry>,
    proton: Option<&ProtonInstall>,
) -> Vec<OptionRow> {
    choices()
        .into_iter()
        .filter_map(|choice| {
            let availability = availability(&choice, profile, gpu, compat, proton);
            let selected = choice.is_selected(profile);
            (availability.is_available() || selected).then_some(OptionRow {
                choice,
                availability,
                selected,
            })
        })
        .collect()
}

/// The choices the matrix hides and the profile has not selected, with reasons.
///
/// A hidden *current* selection is not listed here: it is drawn as a row, so the
/// screen never hides the state it is in.
pub fn hidden(
    profile: &Profile,
    gpu: &GpuProfile,
    compat: Option<&CompatEntry>,
    proton: Option<&ProtonInstall>,
) -> Vec<HiddenRow> {
    choices()
        .into_iter()
        .filter_map(|choice| {
            if choice.is_selected(profile) {
                return None;
            }
            match availability(&choice, profile, gpu, compat, proton) {
                Availability::Available => None,
                Availability::Hidden { reason } => Some(HiddenRow {
                    section: choice.section(),
                    label: choice.label(),
                    reason,
                }),
            }
        })
        .collect()
}

/// The matrix's verdict for one choice.
///
/// A choice with no field (diagnostics, the "off"/"automatic" rows) is always
/// available; everything else asks the one capability function the compiler
/// consumes.
pub fn availability(
    choice: &Choice,
    profile: &Profile,
    gpu: &GpuProfile,
    compat: Option<&CompatEntry>,
    proton: Option<&ProtonInstall>,
) -> Availability {
    match choice.field() {
        None => Availability::Available,
        Some(field) => capability::availability(field, profile, gpu, compat, proton),
    }
}

/// The screen's edit state: a working draft, the last applied profile, and the
/// in-memory undo stack.
///
/// The three verbs the ticket names live here and nowhere else: [`apply`]
/// commits the draft, [`revert`] throws unsaved edits away, and [`undo`] walks
/// the stack back one press at a time. Every edit records the previous draft
/// first, so an undo always returns to a state the user actually saw.
///
/// [`apply`]: GraphicsEditor::apply
/// [`revert`]: GraphicsEditor::revert
/// [`undo`]: GraphicsEditor::undo
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsEditor {
    /// The last profile the user applied (the durable value on disk).
    applied: Profile,
    /// The profile the screen is editing.
    draft: Profile,
    /// Previous drafts, newest last.
    undo: Vec<Profile>,
}

impl GraphicsEditor {
    /// Start editing `profile`, with nothing applied and nothing to undo.
    pub fn new(profile: Profile) -> Self {
        Self {
            applied: profile.clone(),
            draft: profile,
            undo: Vec::new(),
        }
    }

    /// The profile being edited.
    pub fn draft(&self) -> &Profile {
        &self.draft
    }

    /// The last applied profile.
    pub fn applied(&self) -> &Profile {
        &self.applied
    }

    /// Whether the draft differs from the applied profile.
    pub fn is_dirty(&self) -> bool {
        self.draft != self.applied
    }

    /// Whether there is anything to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// How many undo steps are stacked.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// Press a choice: record the current draft, then write the choice in.
    pub fn press(&mut self, choice: &Choice) {
        let mut next = self.draft.clone();
        choice.press(&mut next);
        self.commit_edit(next);
    }

    /// Reset the graphics fields to the safe defaults, keeping the profile's
    /// identity (game, name), its Proton choice and its wrapper chain.
    pub fn reset_to_safe_defaults(&mut self) {
        let mut next = Profile::new(self.draft.game.clone(), self.draft.name.clone());
        next.proton = self.draft.proton.clone();
        next.wrappers = self.draft.wrappers.clone();
        // The advanced env overrides are not on this screen, so they survive a
        // reset; everything the screen owns does not.
        next.env = self.draft.env.clone();
        self.commit_edit(next);
    }

    /// Walk the undo stack back one press. Returns whether anything changed.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.draft = previous;
        true
    }

    /// Throw the unsaved edits away, returning the draft to the applied profile.
    ///
    /// This is the screen's "revert": the in-memory answer. The *durable* revert
    /// of files a Tier B injection wrote is the mod manifest
    /// ([`durable_revert`]); reverting a profile never touches the game tree.
    pub fn revert(&mut self) -> bool {
        if !self.is_dirty() && self.undo.is_empty() {
            return false;
        }
        self.draft = self.applied.clone();
        self.undo.clear();
        true
    }

    /// Validate the draft and commit it as the applied profile.
    ///
    /// Validation runs on the draft before anything is committed, so a refused
    /// apply leaves both the draft and the applied profile untouched. A
    /// successful apply clears the undo stack: there is nothing left to undo to.
    pub fn apply(&mut self) -> Result<(), String> {
        self.draft.validate().map_err(|error| error.to_string())?;
        self.applied = self.draft.clone();
        self.undo.clear();
        Ok(())
    }

    /// Replace both the draft and the applied profile (an external write).
    pub fn reload(&mut self, profile: Profile) {
        self.applied = profile.clone();
        self.draft = profile;
        self.undo.clear();
    }

    /// Record the edit: push the current draft onto the undo stack, then adopt
    /// `next`. A no-op edit (a press that changes nothing) is not recorded, so
    /// the undo stack only ever holds real steps.
    fn commit_edit(&mut self, next: Profile) {
        if next == self.draft {
            return;
        }
        self.undo.push(self.draft.clone());
        self.draft = next;
    }
}

/// One focusable control on the screen, in draw order.
///
/// The controller's ring walks this list: every option row, then the action
/// buttons the current state offers. Keeping it here (rather than in the view)
/// means the ring's order and the conditions that add a button are unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The option row at this index into [`rows`].
    Row(usize),
    Apply,
    Revert,
    Undo,
    Reset,
    Restore,
}

/// The screen's focusable controls, in order.
///
/// Every row, then **Apply** (always present; disabled until the draft is
/// dirty), **Revert** (only when there is something to throw away), **Undo**
/// (only with a step to undo), **Reset** (always) and **Restore** (only when a
/// mod manifest is installed for the game).
pub fn controls(rows: usize, can_undo: bool, dirty: bool, has_manifest: bool) -> Vec<Control> {
    let mut out: Vec<Control> = (0..rows).map(Control::Row).collect();
    out.push(Control::Apply);
    if dirty {
        out.push(Control::Revert);
    }
    if can_undo {
        out.push(Control::Undo);
    }
    out.push(Control::Reset);
    if has_manifest {
        out.push(Control::Restore);
    }
    out
}

/// The anti-cheat injection gate for a draft profile.
///
/// The screen consults this before Apply: a draft that requests injection on a
/// game known to use Easy Anti-Cheat or BattlEye needs an explicit
/// acknowledgement first, and `Unknown` never hard-gates (the plan's rule). The
/// decision and its message come straight from `platform::anticheat`, so the
/// warning the screen shows is the gate's own words.
pub fn injection_gate(profile: &Profile, anticheat: AntiCheat, acknowledged: bool) -> GateDecision {
    anticheat::gate(anticheat, profile.requests_injection(), acknowledged)
}

/// The durable revert of a Tier B injection: the mod manifest a game directory
/// carries, summarised for the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableRevert {
    /// The mod that is installed, e.g. `optiscaler`.
    pub mod_name: String,
    /// How many files the manifest records.
    pub files: usize,
}

/// Read the mod manifest for a game directory, if one is installed.
///
/// `Ok(None)` means no manifest; `Err` means a manifest exists but is malformed,
/// which the screen surfaces rather than hiding.
pub fn durable_revert(install_dir: &Path) -> Result<Option<DurableRevert>, String> {
    match pitagames_mods::ModManifest::load(install_dir) {
        Ok(None) => Ok(None),
        Ok(Some(manifest)) => Ok(Some(DurableRevert {
            mod_name: manifest.mod_name,
            files: manifest.entries.len(),
        })),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::profile::{
        ModelSource, NrBackend, NrModel, NrProcessing, NrRoute, PkgArch,
    };
    use pitagames_core::{GameId, Store};
    use pitagames_platform::capability::reason;
    use pitagames_platform::compat::Version;
    use pitagames_platform::proton::{ProtonKind, ProtonSource};
    use std::path::PathBuf;

    const CYBERPUNK: &str = r#"
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
anticheat = "none"
verified = ["rdna4"]

[upscaling]
supported = ["dlss", "fsr3", "xess"]
preferred_dll = "dbghelp.dll"
helixsr = { modes = ["scaler", "fsr31_dropin"], preferred = "scaler" }

[neural_rendering]
backend = "auto"
route = "optiscaler"
min_mesa = "26.2"
model_scale_default = { rdna4 = 100, rdna3 = 50 }
requires_64bit = true
"#;

    fn game() -> GameId {
        GameId::new(Store::Steam, "1091500")
    }

    fn profile() -> Profile {
        Profile::new(game(), "Graphics")
    }

    fn gpu(arch: pitagames_platform::gpu::GpuArch) -> GpuProfile {
        GpuProfile {
            vendor: arch.vendor(),
            arch,
            device_id: None,
            name: None,
            mesa: Some(Version::parse("27.0.0").unwrap()),
            vram_bytes: None,
            ntsync: false,
            hdr: pitagames_platform::gpu::HdrSupport::Unknown,
            warnings: Vec::new(),
        }
    }

    fn cachyos(version: &str) -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
            name: "proton-cachyos".to_string(),
            kind: ProtonKind::CachyOs,
            version: Some(format!("proton-cachyos {version}")),
            source: ProtonSource::SystemCompatibilityTools,
        }
    }

    fn cyberpunk() -> CompatEntry {
        CompatEntry::from_toml(CYBERPUNK).unwrap()
    }

    // -- The rows ----------------------------------------------------------

    #[test]
    fn only_available_choices_are_rows_and_the_rest_carry_a_reason() {
        let profile = profile();
        let gpu = gpu(pitagames_platform::gpu::GpuArch::Rdna1);
        let visible = rows(
            &profile,
            &gpu,
            Some(&cyberpunk()),
            Some(&cachyos("11.0-20260516")),
        );
        let hidden = hidden(
            &profile,
            &gpu,
            Some(&cyberpunk()),
            Some(&cachyos("11.0-20260516")),
        );

        // FSR 4 is not offered on RDNA 1, so its row is absent and its reason
        // is reported.
        assert!(
            !visible
                .iter()
                .any(|row| row.choice == Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None })),
            "an unavailable option is not a row"
        );
        let fsr4 = hidden
            .iter()
            .find(|row| row.label == "FSR 4 upgrade")
            .expect("FSR 4 is hidden with a reason");
        assert_eq!(fsr4.reason, reason::F4_NEEDS_RDNA3_PLUS);
        assert_eq!(fsr4.section, Section::Upscaling);

        // HelixSR is offered on RDNA 1, so its row is drawn.
        assert!(
            visible
                .iter()
                .any(|row| row.choice == Choice::Upscaler(Upscaler::HlxDlss))
        );
    }

    #[test]
    fn the_off_and_automatic_rows_are_never_hidden() {
        let profile = profile();
        let gpu = gpu(pitagames_platform::gpu::GpuArch::Unknown);
        let visible = rows(&profile, &gpu, None, None);
        for choice in [
            Choice::Upscaler(Upscaler::Off),
            Choice::Upscaler(Upscaler::Native),
            Choice::FrameGen(FrameGen::Off),
            Choice::Injection(Injection::Auto),
            Choice::Injection(Injection::Off),
        ] {
            assert!(
                visible.iter().any(|row| row.choice == choice),
                "{choice:?} must always be offered"
            );
        }
        // And every diagnostic toggle is offered.
        for toggle in DiagnosticToggle::ALL {
            assert!(
                visible
                    .iter()
                    .any(|row| row.choice == Choice::Diagnostic(toggle)),
                "{toggle:?}"
            );
        }
    }

    #[test]
    fn the_current_selection_is_always_drawn_even_when_the_matrix_hides_it() {
        // The profile asks for FSR 4, but the card cannot offer it: the row is
        // drawn (selected, hidden) so the state is representable, and it is not
        // duplicated in the hidden list.
        let mut profile = profile();
        profile.upscaler = Upscaler::Fsr4Upgrade { version: None };
        let gpu = gpu(pitagames_platform::gpu::GpuArch::Rdna1);

        let visible = rows(&profile, &gpu, Some(&cyberpunk()), None);
        let row = visible
            .iter()
            .find(|row| row.choice == Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None }))
            .expect("the current selection is drawn");
        assert!(row.selected);
        assert!(!row.is_available());
        assert_eq!(row.reason(), Some(reason::F4_NEEDS_RDNA3_PLUS));

        let hidden = hidden(&profile, &gpu, Some(&cyberpunk()), None);
        assert!(
            !hidden.iter().any(|row| row.label == "FSR 4 upgrade"),
            "a drawn current selection is not also listed as hidden"
        );
    }

    #[test]
    fn the_single_slot_hides_the_other_owner_through_the_matrix() {
        // DLSSNR is enabled: the injected upscalers are hidden by the same
        // `availability` call the compiler consumes (ADR-0023), not by a second
        // rule in the screen.
        let mut profile = profile();
        profile.neural_rendering = nr();
        let gpu = gpu(pitagames_platform::gpu::GpuArch::Rdna4);
        let compat = cyberpunk();
        let proton = cachyos("11.0-20260516");

        let hidden = hidden(&profile, &gpu, Some(&compat), Some(&proton));
        let dlss_to_fsr4 = hidden
            .iter()
            .find(|row| row.label == "DLSS to FSR 4 (OptiScaler)")
            .expect("an injected upscaler is hidden while DLSSNR owns the slot");
        assert_eq!(dlss_to_fsr4.reason, reason::NR_OWNS_SLOT);

        // And the DLSSNR injection row itself stays available.
        let visible = rows(&profile, &gpu, Some(&compat), Some(&proton));
        let nr_row = visible
            .iter()
            .find(|row| row.choice == Choice::Injection(Injection::DlssNr))
            .expect("DLSSNR's own row is drawn");
        assert!(nr_row.is_available());
    }

    #[test]
    fn the_sections_cover_every_choice_and_keep_their_order() {
        let profile = profile();
        let gpu = gpu(pitagames_platform::gpu::GpuArch::Rdna4);
        let all = choices();
        assert_eq!(all[0].section(), Section::Upscaling);
        assert_eq!(all.last().unwrap().section(), Section::Diagnostics);
        // The visible rows are a stable section-ordered subset of the choices.
        let visible = rows(
            &profile,
            &gpu,
            Some(&cyberpunk()),
            Some(&cachyos("11.0-20260516")),
        );
        let mut last = Section::Upscaling;
        for row in &visible {
            assert!(row.choice.section() >= last, "sections stay in order");
            last = row.choice.section();
        }
    }

    #[test]
    fn a_diagnostic_press_toggles_and_a_selection_press_replaces() {
        let mut profile = profile();
        let dxvk = Choice::Diagnostic(DiagnosticToggle::DxvkHud);
        assert!(!dxvk.is_selected(&profile));
        dxvk.press(&mut profile);
        assert!(profile.diagnostics.dxvk_hud);
        assert!(dxvk.is_selected(&profile));
        dxvk.press(&mut profile);
        assert!(!profile.diagnostics.dxvk_hud);

        let fsr4 = Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None });
        fsr4.press(&mut profile);
        assert_eq!(profile.upscaler, Upscaler::Fsr4Upgrade { version: None });
        assert!(fsr4.is_selected(&profile));
    }

    // -- The editor: apply / revert / undo / reset -------------------------

    #[test]
    fn a_fresh_editor_is_clean_and_has_nothing_to_undo() {
        let editor = GraphicsEditor::new(profile());
        assert!(!editor.is_dirty());
        assert!(!editor.can_undo());
        assert_eq!(editor.undo_depth(), 0);
        assert_eq!(editor.applied(), editor.draft());
    }

    #[test]
    fn a_press_dirties_the_draft_and_stacks_one_undo_step() {
        let mut editor = GraphicsEditor::new(profile());
        editor.press(&Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None }));
        assert!(editor.is_dirty());
        assert!(editor.can_undo());
        assert_eq!(editor.undo_depth(), 1);
        assert_eq!(
            editor.draft().upscaler,
            Upscaler::Fsr4Upgrade { version: None }
        );
        // The applied profile is untouched until apply.
        assert_eq!(editor.applied().upscaler, Upscaler::Native);
    }

    #[test]
    fn undo_walks_back_each_press_and_stops_at_the_start() {
        let mut editor = GraphicsEditor::new(profile());
        editor.press(&Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None }));
        editor.press(&Choice::FrameGen(FrameGen::Fsr3));
        editor.press(&Choice::Diagnostic(DiagnosticToggle::DxvkHud));
        assert_eq!(editor.undo_depth(), 3);

        assert!(editor.undo());
        assert!(!editor.draft().diagnostics.dxvk_hud);
        assert_eq!(editor.draft().frame_gen, FrameGen::Fsr3);

        assert!(editor.undo());
        assert_eq!(editor.draft().frame_gen, FrameGen::Off);
        assert_eq!(
            editor.draft().upscaler,
            Upscaler::Fsr4Upgrade { version: None }
        );

        assert!(editor.undo());
        assert_eq!(editor.draft().upscaler, Upscaler::Native);
        assert!(!editor.can_undo());
        assert!(!editor.undo(), "nothing left to undo");
    }

    #[test]
    fn a_press_that_changes_nothing_is_not_recorded() {
        let mut editor = GraphicsEditor::new(profile());
        // `Off` is already the frame-gen default: pressing it is a no-op.
        editor.press(&Choice::FrameGen(FrameGen::Off));
        assert!(!editor.is_dirty());
        assert!(!editor.can_undo());
    }

    #[test]
    fn revert_throws_unsaved_edits_away_and_clears_the_stack() {
        let mut editor = GraphicsEditor::new(profile());
        editor.press(&Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None }));
        editor.press(&Choice::FrameGen(FrameGen::Fsr4Mlfg));
        assert!(editor.is_dirty());

        assert!(editor.revert());
        assert!(!editor.is_dirty());
        assert!(!editor.can_undo());
        assert_eq!(editor.draft().upscaler, Upscaler::Native);

        // Reverting a clean editor does nothing.
        assert!(!editor.revert());
    }

    #[test]
    fn apply_commits_the_draft_and_clears_the_undo_stack() {
        let mut editor = GraphicsEditor::new(profile());
        editor.press(&Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None }));
        assert!(editor.is_dirty());

        editor.apply().unwrap();
        assert!(!editor.is_dirty());
        assert!(!editor.can_undo());
        assert_eq!(
            editor.applied().upscaler,
            Upscaler::Fsr4Upgrade { version: None }
        );
        // A revert after apply has nothing to return to but the applied value.
        assert!(!editor.revert());
    }

    #[test]
    fn a_refused_apply_leaves_both_the_draft_and_the_applied_profile_untouched() {
        // Two injection owners is the plan's conflict; `Profile::validate`
        // refuses it, so apply must too — and must not commit the half-built
        // state.
        let mut editor = GraphicsEditor::new(profile());
        editor.press(&Choice::Injection(Injection::Manual));
        editor.press(&Choice::Injection(Injection::DlssNr));
        assert!(editor.is_dirty());
        assert!(editor.apply().is_err());
        assert_eq!(editor.applied().injection, Injection::Auto);
        assert!(
            editor.is_dirty(),
            "the draft is kept so the user can fix it"
        );
    }

    #[test]
    fn reset_to_safe_defaults_keeps_the_identity_and_proton_but_drops_the_graphics() {
        let mut profile = profile();
        profile.proton = pitagames_core::ProtonRef::CachyOs;
        profile.wrappers.mangohud = true;
        profile.upscaler = Upscaler::Fsr4Upgrade { version: None };
        profile.frame_gen = FrameGen::Fsr3;
        profile.injection = Injection::Manual;
        profile.neural_rendering = nr();
        profile.diagnostics.dxvk_hud = true;

        let mut editor = GraphicsEditor::new(profile);
        editor.reset_to_safe_defaults();

        let draft = editor.draft();
        assert_eq!(draft.game, game());
        assert_eq!(draft.name, "Graphics");
        assert_eq!(draft.proton, pitagames_core::ProtonRef::CachyOs);
        assert!(draft.wrappers.mangohud);
        assert_eq!(draft.upscaler, Upscaler::Native);
        assert_eq!(draft.frame_gen, FrameGen::Off);
        assert_eq!(draft.injection, Injection::Auto);
        assert!(!draft.neural_rendering.is_enabled());
        assert!(!draft.diagnostics.dxvk_hud);
        // The reset itself is an undoable step.
        assert!(editor.can_undo());
    }

    #[test]
    fn reload_replaces_both_profiles_and_clears_the_stack() {
        let mut editor = GraphicsEditor::new(profile());
        editor.press(&Choice::Upscaler(Upscaler::Fsr4Upgrade { version: None }));
        let mut next = profile();
        next.frame_gen = FrameGen::Fsr4Mlfg;
        editor.reload(next.clone());
        assert_eq!(editor.draft(), &next);
        assert_eq!(editor.applied(), &next);
        assert!(!editor.can_undo());
        assert!(!editor.is_dirty());
    }

    // -- The focus controls ------------------------------------------------

    #[test]
    fn the_controls_are_every_row_then_the_actions_the_state_offers() {
        // A clean editor with no manifest: Apply and Reset only.
        assert_eq!(
            controls(2, false, false, false),
            [
                Control::Row(0),
                Control::Row(1),
                Control::Apply,
                Control::Reset,
            ]
        );
        // Dirty and undoable with a manifest: Revert, Undo and Restore appear.
        assert_eq!(
            controls(1, true, true, true),
            [
                Control::Row(0),
                Control::Apply,
                Control::Revert,
                Control::Undo,
                Control::Reset,
                Control::Restore,
            ]
        );
        // Undo without a dirty draft (an applied step): Undo shows, Revert does not.
        assert_eq!(
            controls(0, true, false, false),
            [Control::Apply, Control::Undo, Control::Reset]
        );
    }

    // -- The durable revert ------------------------------------------------

    #[test]
    fn the_durable_revert_reads_the_mod_manifest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(durable_revert(dir.path()).unwrap(), None);

        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let dll = staging.join("OptiScaler.dll");
        std::fs::write(&dll, b"dll").unwrap();
        let game_dir = dir.path().join("game");
        std::fs::create_dir_all(&game_dir).unwrap();
        pitagames_mods::ModManifest::install(
            &game_dir,
            "optiscaler",
            &[pitagames_mods::ModSource::new("dxgi.dll", dll)],
        )
        .unwrap();

        let summary = durable_revert(&game_dir).unwrap().expect("a manifest");
        assert_eq!(summary.mod_name, "optiscaler");
        assert_eq!(summary.files, 1);
    }

    // -- The anti-cheat gate -----------------------------------------------

    #[test]
    fn the_injection_gate_only_engages_when_the_draft_requests_injection() {
        let profile = profile();
        assert_eq!(
            injection_gate(&profile, AntiCheat::Eac, false),
            GateDecision::NotRequested,
            "a profile that injects nothing is never gated"
        );

        let mut injected = profile.clone();
        injected.upscaler = Upscaler::DlssToFsr4;
        assert_eq!(
            injection_gate(&injected, AntiCheat::Eac, false),
            GateDecision::ConfirmationRequired {
                anticheat: AntiCheat::Eac
            }
        );
        assert!(injection_gate(&injected, AntiCheat::Eac, true).allows());
        // `Unknown` never hard-gates.
        assert!(injection_gate(&injected, AntiCheat::Unknown, false).allows());
    }

    fn nr() -> pitagames_core::profile::NeuralRendering {
        pitagames_core::profile::NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.8".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 100,
            processing: NrProcessing::RenoDx,
            pkg_arch: PkgArch::X86_64,
            captured_env: Default::default(),
            model_source: ModelSource::CachedInPackage,
        }
    }
}
