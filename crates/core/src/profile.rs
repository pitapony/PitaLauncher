//! Per-game launch profiles and graphics settings.
//!
//! A [`Profile`] is pure data: the UI edits it, the database persists it, and
//! `platform::envc` compiles it into a [`crate::LaunchSpec`] together with GPU
//! and Proton information. This module deliberately does no environment
//! compilation and no file I/O, so profile edits stay testable without a GPU.
//!
//! Serde shape: enums use internally tagged representations (`{"kind": ...}`)
//! so profiles stay readable in the database and survive field additions.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::store::GameId;

/// A named launch configuration for one game.
///
/// A game always has at least one profile; the default profile is created when
/// the game is first shown. Switching a profile in the UI writes the compiled
/// spec to `profiles/steam-<appid>.json`, which the shim reads at launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub game: GameId,
    pub name: String,
    #[serde(default)]
    pub proton: ProtonRef,
    #[serde(default)]
    pub wrappers: WrapperChain,
    #[serde(default)]
    pub upscaler: Upscaler,
    #[serde(default)]
    pub frame_gen: FrameGen,
    #[serde(default)]
    pub injection: Injection,
    #[serde(default)]
    pub injection_dll: InjectionDll,
    #[serde(default)]
    pub neural_rendering: NeuralRendering,
    /// Line-preserving ini overrides, e.g. `Upscalers.Dx12Upscaler -> fsr4`.
    #[serde(default)]
    pub ini_overrides: BTreeMap<String, String>,
    /// Advanced environment overrides. Validated by the env compiler, not here.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub diagnostics: Diagnostics,
}

impl Profile {
    /// A profile with the safe defaults: no injection, no wrappers, no tuning.
    pub fn new(game: GameId, name: impl Into<String>) -> Self {
        Self {
            game,
            name: name.into(),
            proton: ProtonRef::default(),
            wrappers: WrapperChain::default(),
            upscaler: Upscaler::default(),
            frame_gen: FrameGen::default(),
            injection: Injection::default(),
            injection_dll: InjectionDll::default(),
            neural_rendering: NeuralRendering::default(),
            ini_overrides: BTreeMap::new(),
            env: BTreeMap::new(),
            diagnostics: Diagnostics::default(),
        }
    }

    /// Reject combinations that would make the env compiler emit contradictory
    /// settings. The graphics UI calls this before saving; the database does
    /// the same on write, so a corrupt profile can never reach a launch.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.name.trim().is_empty() {
            return Err(ProfileError::EmptyName);
        }

        self.wrappers.validate()?;
        self.neural_rendering.validate()?;

        if self.neural_rendering.is_enabled() {
            // DLSSNR ships its own OptiScaler. Any profile setting that makes
            // Proton or us deploy another one is a hard conflict (the plan's
            // "one injection slot" rule), not a warning.
            if self.upscaler.uses_injection() {
                return Err(ProfileError::Conflict {
                    left: "neural rendering",
                    right: "an injected upscaler",
                });
            }

            match self.injection {
                Injection::EnvOnly | Injection::Manual | Injection::HelixSr => {
                    return Err(ProfileError::Conflict {
                        left: "neural rendering",
                        right: self.injection.describe(),
                    });
                }
                Injection::Auto | Injection::DlssNr | Injection::Off => {}
            }
        } else if self.injection == Injection::DlssNr {
            return Err(ProfileError::Conflict {
                left: "DLSSNR injection",
                right: "neural rendering is off",
            });
        }

        Ok(())
    }
}

/// Which Proton build starts the game.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProtonRef {
    /// Let `platform::proton` pick the best available build for the GPU.
    #[default]
    Auto,
    /// Whatever Steam's per-game compatibility setting says.
    SteamDefault,
    /// Proton-CachyOS from the distribution repository.
    CachyOs,
    /// GE-Proton from `compatibilitytools.d`.
    GeProton,
    /// An explicit directory or binary. A struct variant keeps the JSON shape
    /// stable and readable (`{"kind": "path", "path": "..."}`).
    Path { path: PathBuf },
}

/// Ordered process wrappers, applied outermost-first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WrapperChain {
    pub gamemode: bool,
    pub mangohud: bool,
    pub gamescope: Option<GamescopeOptions>,
    /// Arbitrary wrapper programs from `program + args`, never a shell string.
    pub custom: Vec<WrapperStep>,
}

impl WrapperChain {
    /// `true` when the chain would add no process in front of the game.
    pub fn is_empty(&self) -> bool {
        !self.gamemode && !self.mangohud && self.gamescope.is_none() && self.custom.is_empty()
    }

    /// Reject chains that could not be exec'd safely.
    ///
    /// The compiler expands wrappers into `program + args` and the shim execs
    /// the first program directly, so an empty program is a launch failure, not
    /// a cosmetic problem. NUL bytes cannot cross `execve`, and a zero gamescope
    /// dimension is always a typo rather than a request. Validation lives here
    /// (not in the UI) so a profile loaded from the database or imported from a
    /// file is held to the same rules.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if let Some(gamescope) = &self.gamescope {
            gamescope.validate()?;
        }
        for (index, step) in self.custom.iter().enumerate() {
            if step.program.as_os_str().is_empty() {
                return Err(ProfileError::EmptyWrapperProgram { index });
            }
            if contains_nul(&step.program.to_string_lossy()) {
                return Err(ProfileError::WrapperNul { index });
            }
            if step.args.iter().any(|arg| arg.contains('\0')) {
                return Err(ProfileError::WrapperNul { index });
            }
        }
        Ok(())
    }
}

/// One `program + args` step in a wrapper chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrapperStep {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Per-game gamescope flags.
///
/// Ignored under `PITAGAMES_SESSION=1`: the console session is already wrapped
/// by gamescope, and nesting it is a bug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GamescopeOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub refresh_hz: Option<u32>,
    pub fullscreen: bool,
    pub extra_args: Vec<String>,
}

impl Default for GamescopeOptions {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            refresh_hz: None,
            fullscreen: true,
            extra_args: Vec::new(),
        }
    }
}

impl GamescopeOptions {
    /// Reject dimensions that gamescope would interpret as an error.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let fields = [
            ("width", self.width),
            ("height", self.height),
            ("refresh", self.refresh_hz),
        ];
        for (field, value) in fields {
            if value == Some(0) {
                return Err(ProfileError::GamescopeZero { field });
            }
        }
        if self.extra_args.iter().any(|arg| arg.contains('\0')) {
            return Err(ProfileError::GamescopeNul);
        }
        Ok(())
    }
}

fn contains_nul(text: &str) -> bool {
    text.contains('\0')
}

/// How the game's upscaling is produced.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Upscaler {
    /// No upscaling; the game's own setting decides.
    Off,
    /// Leave the game/Proton defaults alone.
    #[default]
    Native,
    /// Proton's FSR 4 upgrade hook, optionally pinned to a version.
    Fsr4Upgrade {
        version: Option<String>,
    },
    /// The RDNA 3-only FSR 4 upgrade path (experimental).
    Fsr4Rdna3 {
        version: Option<String>,
    },
    DlssToFsr4,
    DlssToFsr31,
    DlssToXess,
    XessUpgrade,
    Ffx3Upgrade,
    Ffx4Upgrade,
    /// HelixSR Scaler presenting NVIDIA's DLSS network.
    HlxDlss,
    /// HelixSR Scaler presenting HLX FSR 4.
    HlxFsr4,
    /// HelixSR FSR 3.1 drop-in; replaces the game's FSR DLL.
    HlxFsr4DropIn,
    /// gamescope's FSR 1 fallback.
    SystemFsr,
}

impl Upscaler {
    /// Whether this choice needs a DLL-injection mechanism (Proton's env hook,
    /// OptiScaler files, or HelixSR). Used by the "one injection slot" checks.
    pub const fn uses_injection(&self) -> bool {
        !matches!(self, Self::Off | Self::Native | Self::SystemFsr)
    }
}

/// Frame generation, independent from [`Upscaler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameGen {
    #[default]
    Off,
    /// Proton-CachyOS ML frame generation.
    Fsr4Mlfg,
    /// OptiScaler-driven FSR 3 frame generation.
    Fsr3,
}

/// Who owns the injection slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Injection {
    /// Resolve from the GPU/Proton capabilities.
    #[default]
    Auto,
    /// Tier A: Proton deploys and validates the DLLs, the launcher only sets env.
    EnvOnly,
    /// Tier B: the launcher manages OptiScaler files for this game.
    Manual,
    /// HelixSR owns the files (its own OptiScaler copy).
    HelixSr,
    /// DLSSNR-AMD owns the files (it ships OptiScaler too).
    DlssNr,
    /// No injection at all.
    Off,
}

impl Injection {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Auto => "automatic injection",
            Self::EnvOnly => "Tier A (env-only) injection",
            Self::Manual => "manual OptiScaler injection",
            Self::HelixSr => "HelixSR injection",
            Self::DlssNr => "DLSSNR injection",
            Self::Off => "no injection",
        }
    }
}

/// The DLL name a manual injection displaces, in preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionDll {
    #[default]
    Dxgi,
    D3d12,
    Dbghelp,
    Winmm,
    Version,
}

/// DLSS 5 neural rendering on RDNA 4 (and the RDNA 3 fork).
///
/// Independent from [`Upscaler`]: DLSSNR adds a neural-rendering pass, while
/// OptiScaler still performs the upscaling.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NeuralRendering {
    #[default]
    Off,
    DlssNrAmd {
        backend: NrBackend,
        /// Pinned release of the selected backend.
        version: String,
        route: NrRoute,
        model: NrModel,
        /// Network resolution percentage; the main cost lever on RDNA 3.
        model_scale: u8,
        processing: NrProcessing,
        pkg_arch: PkgArch,
        /// Launch-option env printed by the upstream installer and captured by
        /// `mods::capture`, already allowlist-validated at capture time.
        #[serde(default)]
        captured_env: BTreeMap<String, String>,
        model_source: ModelSource,
    },
}

impl NeuralRendering {
    pub const MODEL_SCALE_MIN: u8 = 25;
    pub const MODEL_SCALE_MAX: u8 = 100;

    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::DlssNrAmd { .. })
    }

    /// Validate the parts of neural rendering that do not depend on the game
    /// or the GPU. Capability gates (Mesa, VRAM, 64-bit) live in `platform`.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let Self::DlssNrAmd {
            route, model_scale, ..
        } = self
        else {
            return Ok(());
        };

        if !(Self::MODEL_SCALE_MIN..=Self::MODEL_SCALE_MAX).contains(model_scale) {
            return Err(ProfileError::ModelScale {
                value: *model_scale,
                min: Self::MODEL_SCALE_MIN,
                max: Self::MODEL_SCALE_MAX,
            });
        }

        // v1 implements the OptiScaler route only. The ReShade routes are
        // future work and must be reported as unavailable, never installed.
        if *route != NrRoute::Optiscaler {
            return Err(ProfileError::RouteNotAvailable { route: *route });
        }

        Ok(())
    }
}

/// Which DLSSNR backend a GPU architecture is allowed to use.
///
/// Never offer one architecture the other's package: upstream RDNA 4 is
/// validated by us, the RDNA 3 fork is explicitly not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NrBackend {
    /// `mochizuki0323/DLSSNR-AMD` on GitLab (RDNA 4).
    Rdna4Upstream,
    /// `mauri870/DLSSNR-RDNA3` fork (RDNA 3, experimental).
    Rdna3Fork,
}

/// How DLSSNR hooks into the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NrRoute {
    /// Ships OptiScaler + the DLSSNR plugin. The only route in v1.
    Optiscaler,
    ReshadeFeeder,
    ReshadeFeederVulkan,
    ReshadeFeederDx9,
}

impl NrRoute {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Optiscaler => "OptiScaler",
            Self::ReshadeFeeder => "ReShade feeder",
            Self::ReshadeFeederVulkan => "ReShade feeder (Vulkan)",
            Self::ReshadeFeederDx9 => "ReShade feeder (DX9)",
        }
    }
}

/// The NR network variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NrModel {
    CcSmallFp8,
    CcSilverAardwold,
    Int4Mixed,
    SmallFp8Experimental,
}

/// Upstream's two processing styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NrProcessing {
    RenoDx,
    Wilsjo2,
}

/// The package architecture. v1 ships x86_64 only; 32-bit games are unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PkgArch {
    X86_64,
    I686,
}

/// Where the NR model weights come from. They are NVIDIA's property and are
/// never redistributed or copied into the launcher's own directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelSource {
    /// Found in an installed game or Proton prefix.
    ScannedDll { path: PathBuf },
    /// User-provided zip containing exactly `nvngx_dlssnr.dll`.
    UserZip { path: PathBuf },
    /// Already extracted and cached by the upstream package.
    CachedInPackage,
}

/// Diagnostic overlays and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Diagnostics {
    /// `DXVK_HUD=fps`.
    pub dxvk_hud: bool,
    /// `MANGOHUD=1`.
    pub mangohud: bool,
    /// `PROTON_LOG=1` (+ `PROTON_LOG_DIR`).
    pub proton_log: bool,
    /// DXVK's own log.
    pub dxvk_log: bool,
    /// `PROTON_FSR4_INDICATOR=1`.
    pub fsr4_indicator: bool,
    /// `PROTON_DLSS_INDICATOR=1`.
    pub dlss_indicator: bool,
}

/// A profile that cannot be compiled safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("profile name must not be empty")]
    EmptyName,

    #[error("neural rendering model_scale must be within {min}..={max}, got {value}")]
    ModelScale { value: u8, min: u8, max: u8 },

    #[error("neural-rendering route {route:?} is not available in v1")]
    RouteNotAvailable { route: NrRoute },

    #[error("custom wrapper #{index} has an empty program")]
    EmptyWrapperProgram { index: usize },

    #[error("custom wrapper #{index} contains a NUL byte")]
    WrapperNul { index: usize },

    #[error("gamescope {field} must be greater than zero")]
    GamescopeZero { field: &'static str },

    #[error("gamescope arguments contain a NUL byte")]
    GamescopeNul,

    #[error("{left} cannot be combined with {right}")]
    Conflict {
        left: &'static str,
        right: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::str::FromStr;

    fn game() -> GameId {
        GameId::new(Store::Steam, "620")
    }

    fn nr(model_scale: u8) -> NeuralRendering {
        NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.8".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale,
            processing: NrProcessing::RenoDx,
            pkg_arch: PkgArch::X86_64,
            captured_env: BTreeMap::from([(
                "WINEDLLOVERRIDES".to_string(),
                "dxgi=n,b".to_string(),
            )]),
            model_source: ModelSource::ScannedDll {
                path: PathBuf::from("/games/Steam/steamapps/common/Game/nvngx_dlssnr.dll"),
            },
        }
    }

    #[test]
    fn defaults_are_safe_and_serializable() {
        let profile = Profile::new(game(), "Default");
        assert_eq!(profile.upscaler, Upscaler::Native);
        assert_eq!(profile.injection, Injection::Auto);
        assert!(!profile.neural_rendering.is_enabled());
        profile.validate().unwrap();

        let json = serde_json::to_string(&profile).unwrap();
        let parsed: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, profile);
    }

    #[test]
    fn a_fully_configured_profile_round_trips() {
        let mut profile = Profile::new(game(), "FSR 4 + NR");
        profile.proton = ProtonRef::CachyOs;
        profile.upscaler = Upscaler::Fsr4Upgrade {
            version: Some("4.0.3".into()),
        };
        profile.frame_gen = FrameGen::Fsr4Mlfg;
        profile.injection = Injection::Off;
        profile.injection_dll = InjectionDll::Winmm;
        profile.wrappers = WrapperChain {
            gamemode: true,
            mangohud: true,
            gamescope: Some(GamescopeOptions {
                width: Some(2560),
                height: Some(1440),
                refresh_hz: Some(165),
                fullscreen: true,
                extra_args: vec!["--hdr-enabled".into()],
            }),
            custom: vec![WrapperStep {
                program: PathBuf::from("/usr/bin/mangohud"),
                args: vec!["--dls".into()],
            }],
        };
        profile.diagnostics.dxvk_hud = true;
        profile.diagnostics.fsr4_indicator = true;
        profile.env.insert("DXVK_HDR".into(), "1".into());
        profile
            .ini_overrides
            .insert("Upscalers.Dx12Upscaler".into(), "fsr4".into());

        let json = serde_json::to_string(&profile).unwrap();
        // The serde shape is part of the on-disk contract.
        assert!(json.contains(r#""kind":"fsr4_upgrade""#), "{json}");
        assert!(json.contains(r#""kind":"cachy_os""#), "{json}");
        assert!(json.contains(r#""width":2560"#), "{json}");

        let parsed: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, profile);
        parsed.validate().unwrap();
    }

    #[test]
    fn neural_rendering_round_trips() {
        let mut profile = Profile::new(game(), "NR");
        profile.neural_rendering = nr(50);
        profile.upscaler = Upscaler::Native;

        let json = serde_json::to_string(&profile.neural_rendering).unwrap();
        assert!(json.contains(r#""mode":"dlss_nr_amd""#), "{json}");
        assert!(json.contains(r#""kind":"scanned_dll""#), "{json}");

        let parsed: NeuralRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, nr(50));
    }

    #[test]
    fn model_scale_outside_the_range_is_rejected() {
        for value in [0, 24, 101, 255] {
            let profile = {
                let mut p = Profile::new(game(), "NR");
                p.neural_rendering = nr(value);
                p
            };
            let error = profile.validate().unwrap_err();
            assert!(
                matches!(error, ProfileError::ModelScale { value: v, .. } if v == value),
                "{value}: {error}"
            );
        }

        for value in [25, 50, 100] {
            let mut profile = Profile::new(game(), "NR");
            profile.neural_rendering = nr(value);
            profile.validate().unwrap();
        }
    }

    #[test]
    fn non_optiscaler_routes_are_not_available_in_v1() {
        for route in [
            NrRoute::ReshadeFeeder,
            NrRoute::ReshadeFeederVulkan,
            NrRoute::ReshadeFeederDx9,
        ] {
            let mut profile = Profile::new(game(), "NR");
            profile.neural_rendering = nr(100);
            let NeuralRendering::DlssNrAmd { route: slot, .. } = &mut profile.neural_rendering
            else {
                unreachable!("nr() builds a DlssNrAmd variant");
            };
            *slot = route;
            assert!(matches!(
                profile.validate(),
                Err(ProfileError::RouteNotAvailable { .. })
            ));
        }
    }

    #[test]
    fn neural_rendering_conflicts_with_injected_upscalers() {
        for upscaler in [
            Upscaler::Fsr4Upgrade { version: None },
            Upscaler::DlssToFsr4,
            Upscaler::HlxDlss,
            Upscaler::HlxFsr4DropIn,
            Upscaler::SystemFsr,
        ] {
            let mut profile = Profile::new(game(), "NR");
            profile.neural_rendering = nr(100);
            profile.upscaler = upscaler.clone();
            let result = profile.validate();
            if upscaler == Upscaler::SystemFsr {
                // gamescope FSR is a compositor effect, not an injection.
                assert!(result.is_ok(), "{upscaler:?}");
            } else {
                assert!(
                    matches!(result, Err(ProfileError::Conflict { .. })),
                    "{upscaler:?}"
                );
            }
        }
    }

    #[test]
    fn neural_rendering_conflicts_with_other_injection_owners() {
        for injection in [Injection::EnvOnly, Injection::Manual, Injection::HelixSr] {
            let mut profile = Profile::new(game(), "NR");
            profile.neural_rendering = nr(100);
            profile.injection = injection;
            assert!(
                matches!(profile.validate(), Err(ProfileError::Conflict { .. })),
                "{injection:?}"
            );
        }

        for injection in [Injection::Auto, Injection::DlssNr, Injection::Off] {
            let mut profile = Profile::new(game(), "NR");
            profile.neural_rendering = nr(100);
            profile.injection = injection;
            profile.validate().unwrap();
        }
    }

    #[test]
    fn dlssnr_injection_requires_neural_rendering() {
        let mut profile = Profile::new(game(), "stale");
        profile.injection = Injection::DlssNr;
        assert!(matches!(
            profile.validate(),
            Err(ProfileError::Conflict { .. })
        ));
    }

    #[test]
    fn wrapper_chains_are_validated() {
        let mut profile = Profile::new(game(), "Wrapped");
        profile.wrappers = WrapperChain {
            gamemode: true,
            mangohud: true,
            gamescope: Some(GamescopeOptions {
                width: Some(2560),
                height: Some(1440),
                refresh_hz: Some(165),
                fullscreen: true,
                extra_args: vec!["--hdr-enabled".into()],
            }),
            custom: vec![WrapperStep {
                program: PathBuf::from("/usr/bin/mangohud"),
                args: vec!["--dls".into()],
            }],
        };
        profile.validate().unwrap();
        assert!(!profile.wrappers.is_empty());

        profile.wrappers.custom.push(WrapperStep {
            program: PathBuf::new(),
            args: Vec::new(),
        });
        assert_eq!(
            profile.validate(),
            Err(ProfileError::EmptyWrapperProgram { index: 1 })
        );

        profile.wrappers.custom[1].program = PathBuf::from("wrapper\0");
        assert_eq!(
            profile.validate(),
            Err(ProfileError::WrapperNul { index: 1 })
        );

        let mut profile = Profile::new(game(), "Gamescope");
        profile.wrappers.gamescope = Some(GamescopeOptions {
            width: Some(0),
            ..GamescopeOptions::default()
        });
        assert_eq!(
            profile.validate(),
            Err(ProfileError::GamescopeZero { field: "width" })
        );
    }

    #[test]
    fn an_empty_wrapper_chain_is_reported() {
        assert!(WrapperChain::default().is_empty());
        assert!(
            !WrapperChain {
                mangohud: true,
                ..WrapperChain::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn empty_names_are_rejected() {
        let profile = Profile::new(game(), "   ");
        assert_eq!(profile.validate(), Err(ProfileError::EmptyName));
    }

    #[test]
    fn unknown_serde_fields_are_ignored_for_forward_compatibility() {
        // A profile written by a newer version must still load here; new
        // settings rely on serde defaults rather than failing the whole row.
        let mut value = serde_json::to_value(Profile::new(game(), "x")).unwrap();
        value["unknown_field"] = serde_json::json!(true);
        let parsed: Profile = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, Profile::new(game(), "x"));
    }

    #[test]
    fn store_ids_parse_for_persisted_profiles() {
        // Sanity check used by the db crate when decoding profile rows.
        assert_eq!(Store::from_str("steam").unwrap(), Store::Steam);
    }
}
