//! In-game key hints, per injection mode, as data.
//!
//! Once a mod is injected, its own in-game menu is driven by upstream's keys
//! (`End` opens DLSSNR's NR Control, `Insert` opens OptiScaler, …). The game page
//! lists them so a player does not have to hunt upstream's README. The keys are
//! **data** (plan §6 item 6): [`HintData::shipped`] parses
//! `data/ingame-hints.toml`, so a changed keybinding is a one-line edit.
//!
//! Each mode also carries an honest one-line **note**. A game's compatibility
//! entry may override that note through `[ingame] notes`, so a title-specific
//! caveat reads on that game's page with no code change; the key list itself is
//! always the mode's shipped list.
//!
//! Framework-free and pure: a profile and a note override in, a [`HintSet`] out.

use pitagames_core::profile::{Injection, Profile, Upscaler};
use serde::{Deserialize, Serialize};

/// The shipped in-game hints, embedded at build time.
pub const SHIPPED: &str = include_str!("../data/ingame-hints.toml");

/// Which injection owns the game's files, and therefore which in-game menu
/// exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InjectionMode {
    /// Tier A: Proton's own env hooks; no in-game menu.
    #[serde(rename = "tier_a")]
    TierA,
    /// Tier B: managed OptiScaler.
    #[serde(rename = "tier_b")]
    TierB,
    /// HelixSR (its own OptiScaler copy).
    #[serde(rename = "helixsr")]
    HelixSr,
    /// DLSSNR-AMD (OptiScaler plus NR Control).
    #[serde(rename = "dlssnr")]
    DlssNr,
}

impl InjectionMode {
    /// Every mode, in the order the data file reads them.
    pub const ALL: [InjectionMode; 4] = [
        InjectionMode::DlssNr,
        InjectionMode::TierB,
        InjectionMode::HelixSr,
        InjectionMode::TierA,
    ];

    /// The stable identifier used in the data file and JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TierA => "tier_a",
            Self::TierB => "tier_b",
            Self::HelixSr => "helixsr",
            Self::DlssNr => "dlssnr",
        }
    }

    /// The human label the panel shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::TierA => "Tier A (Proton env)",
            Self::TierB => "OptiScaler (Tier B)",
            Self::HelixSr => "HelixSR",
            Self::DlssNr => "DLSSNR-AMD",
        }
    }
}

/// Whether an upscaler is one of the HelixSR routes.
const fn is_helixsr(upscaler: &Upscaler) -> bool {
    matches!(
        upscaler,
        Upscaler::HlxDlss | Upscaler::HlxFsr4 | Upscaler::HlxFsr4DropIn
    )
}

/// The injection mode a profile puts the game in, if any.
///
/// Neural rendering and an explicit injection choice decide first; otherwise an
/// injected upscaler implies its tier. A profile that injects nothing has no
/// mode and therefore no hints.
pub fn mode_of(profile: &Profile) -> Option<InjectionMode> {
    if profile.neural_rendering.is_enabled() {
        return Some(InjectionMode::DlssNr);
    }
    match profile.injection {
        Injection::DlssNr => Some(InjectionMode::DlssNr),
        Injection::HelixSr => Some(InjectionMode::HelixSr),
        Injection::Manual => Some(InjectionMode::TierB),
        Injection::EnvOnly => Some(InjectionMode::TierA),
        Injection::Auto | Injection::Off => {
            if is_helixsr(&profile.upscaler) {
                Some(InjectionMode::HelixSr)
            } else if profile.upscaler.uses_injection() {
                Some(InjectionMode::TierB)
            } else {
                None
            }
        }
    }
}

/// One in-game key and what it opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InGameHint {
    /// The key, as upstream prints it (`End`, `Insert`, `Ctrl+F11`).
    pub keys: String,
    /// What the key does.
    pub action: String,
}

/// The shipped keys and note for one mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeHints {
    /// The mode this set belongs to.
    pub mode: InjectionMode,
    /// The keys, in the order the page reads them.
    #[serde(default)]
    pub hints: Vec<InGameHint>,
    /// The one-line note beside the list.
    #[serde(default)]
    pub note: Option<String>,
}

/// The parsed hint file: one [`ModeHints`] per mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HintData {
    /// The mode sets, in file order.
    #[serde(default)]
    pub modes: Vec<ModeHints>,
}

impl HintData {
    /// Parse a hint document.
    pub fn parse(text: &str) -> Result<Self, HintError> {
        let data: Self = toml::from_str(text)?;
        data.validate()?;
        Ok(data)
    }

    /// The shipped hint data.
    ///
    /// The embedded file is compiled data, so a malformed one is a build bug; a
    /// test asserts it parses and covers every mode.
    pub fn shipped() -> Self {
        Self::parse(SHIPPED).expect("the shipped ingame-hints file parses")
    }

    /// The keys and note for a mode, if the file ships them.
    pub fn get(&self, mode: InjectionMode) -> Option<&ModeHints> {
        self.modes.iter().find(|set| set.mode == mode)
    }

    /// Refuse a duplicated mode.
    fn validate(&self) -> Result<(), HintError> {
        for (index, set) in self.modes.iter().enumerate() {
            if self.modes[..index]
                .iter()
                .any(|other| other.mode == set.mode)
            {
                return Err(HintError::DuplicateMode { mode: set.mode });
            }
        }
        Ok(())
    }
}

/// The resolved hints for one game: the mode's keys plus the note to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintSet {
    /// The mode these hints belong to.
    pub mode: InjectionMode,
    /// The keys, in order.
    pub hints: Vec<InGameHint>,
    /// The note to show: the per-game override when present, else the shipped
    /// one.
    pub note: Option<String>,
}

impl HintSet {
    pub fn is_empty(&self) -> bool {
        self.hints.is_empty()
    }
}

/// Resolve a mode's hints, letting a per-game note override the shipped one.
///
/// The key list is always the mode's shipped list; only the note is overridable,
/// because a game can add a caveat but not change what upstream bound.
pub fn hints_for(data: &HintData, mode: InjectionMode, note_override: Option<&str>) -> HintSet {
    let shipped = data.get(mode);
    HintSet {
        mode,
        hints: shipped.map(|set| set.hints.clone()).unwrap_or_default(),
        note: note_override
            .map(str::to_string)
            .or_else(|| shipped.and_then(|set| set.note.clone())),
    }
}

/// The hints for a profile's injection mode, if it injects anything.
pub fn set_for(data: &HintData, profile: &Profile, note_override: Option<&str>) -> Option<HintSet> {
    mode_of(profile).map(|mode| hints_for(data, mode, note_override))
}

/// Why the hint file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum HintError {
    #[error("the in-game hint file could not be read: {source}")]
    Toml {
        #[from]
        source: toml::de::Error,
    },

    #[error("the `{mode:?}` injection mode is listed twice")]
    DuplicateMode { mode: InjectionMode },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::profile::{
        ModelSource, NeuralRendering, NrBackend, NrModel, NrProcessing, NrRoute, PkgArch,
    };
    use pitagames_core::{GameId, Store};

    fn game() -> GameId {
        GameId::new(Store::Steam, "1091500")
    }

    fn profile() -> Profile {
        Profile::new(game(), "Default")
    }

    fn nr(profile: &mut Profile) {
        profile.neural_rendering = NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.9".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 100,
            processing: NrProcessing::RenoDx,
            pkg_arch: PkgArch::X86_64,
            captured_env: Default::default(),
            model_source: ModelSource::CachedInPackage,
        };
    }

    /// A worked hint file, independent of the shipped strings.
    fn data() -> HintData {
        HintData {
            modes: vec![
                ModeHints {
                    mode: InjectionMode::DlssNr,
                    hints: vec![
                        InGameHint {
                            keys: "End".into(),
                            action: "NR Control".into(),
                        },
                        InGameHint {
                            keys: "F6".into(),
                            action: "Toggle NR".into(),
                        },
                    ],
                    note: Some("shipped NR note".into()),
                },
                ModeHints {
                    mode: InjectionMode::TierA,
                    hints: Vec::new(),
                    note: Some("no menu".into()),
                },
            ],
        }
    }

    #[test]
    fn the_mode_follows_the_profile_injection() {
        let mut p = profile();
        assert_eq!(mode_of(&p), None, "nothing injected, no mode");

        p.injection = Injection::Manual;
        assert_eq!(mode_of(&p), Some(InjectionMode::TierB));
        p.injection = Injection::EnvOnly;
        assert_eq!(mode_of(&p), Some(InjectionMode::TierA));
        p.injection = Injection::HelixSr;
        assert_eq!(mode_of(&p), Some(InjectionMode::HelixSr));
        p.injection = Injection::DlssNr;
        assert_eq!(mode_of(&p), Some(InjectionMode::DlssNr));
    }

    #[test]
    fn an_injected_upscaler_implies_its_tier() {
        let mut p = profile();
        p.upscaler = Upscaler::DlssToFsr4;
        assert_eq!(mode_of(&p), Some(InjectionMode::TierB));

        p.upscaler = Upscaler::HlxFsr4;
        assert_eq!(mode_of(&p), Some(InjectionMode::HelixSr));

        p.upscaler = Upscaler::Native;
        assert_eq!(mode_of(&p), None, "a native upscaler injects nothing");
    }

    #[test]
    fn neural_rendering_wins_the_mode() {
        let mut p = profile();
        p.injection = Injection::Manual;
        nr(&mut p);
        assert_eq!(
            mode_of(&p),
            Some(InjectionMode::DlssNr),
            "NR is the owner even if the injection field lags"
        );
    }

    #[test]
    fn a_per_game_note_overrides_the_shipped_one() {
        let data = data();
        let shipped = hints_for(&data, InjectionMode::DlssNr, None);
        assert_eq!(shipped.note.as_deref(), Some("shipped NR note"));
        assert_eq!(shipped.hints.len(), 2);

        let overridden = hints_for(&data, InjectionMode::DlssNr, Some("this game needs x"));
        assert_eq!(overridden.note.as_deref(), Some("this game needs x"));
        assert_eq!(
            overridden.hints, shipped.hints,
            "only the note is overridable, the keys are the mode's"
        );
    }

    #[test]
    fn an_unknown_mode_has_no_hints_but_keeps_the_override() {
        let set = hints_for(&HintData::default(), InjectionMode::TierB, Some("note"));
        assert!(set.is_empty());
        assert_eq!(set.note.as_deref(), Some("note"));
    }

    #[test]
    fn set_for_only_answers_a_profile_that_injects() {
        let data = data();
        assert_eq!(set_for(&data, &profile(), None), None);

        let mut p = profile();
        nr(&mut p);
        let set = set_for(&data, &p, Some("per game")).expect("NR has hints");
        assert_eq!(set.mode, InjectionMode::DlssNr);
        assert_eq!(set.note.as_deref(), Some("per game"));
    }

    // -- the shipped data ---------------------------------------------------

    #[test]
    fn the_shipped_hints_carry_the_plan_keys() {
        let data = HintData::shipped();
        let nr = data.get(InjectionMode::DlssNr).expect("a DLSSNR set");
        let keys: Vec<&str> = nr.hints.iter().map(|hint| hint.keys.as_str()).collect();
        assert_eq!(keys, ["End", "Insert", "F6", "F5", "Ctrl+F11"]);
        // Every mode the enum names ships an entry.
        for mode in InjectionMode::ALL {
            assert!(data.get(mode).is_some(), "{mode:?} is missing");
        }
    }

    #[test]
    fn a_duplicated_mode_is_refused_at_load() {
        let text = r#"
            [[modes]]
            mode = "dlssnr"

            [[modes]]
            mode = "dlssnr"
        "#;
        assert!(matches!(
            HintData::parse(text),
            Err(HintError::DuplicateMode {
                mode: InjectionMode::DlssNr
            })
        ));
    }
}
