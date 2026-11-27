//! The launch compiler.
//!
//! A [`Profile`] is pure data. `platform::envc` turns it into the concrete
//! environment and wrapper chain a launch needs, together with the Proton build
//! it resolved to. The shim, the GUI and the CLI all consume the result through
//! [`ShimProfile`](pitagames_core::ShimProfile), so there is one compiler, not
//! three.
//!
//! **The full M3 compiler.** This build compiles the whole plan §6 graphics
//! table — the Proton FSR/FFX/XeSS upgrade hooks, the OptiScaler routes, HelixSR,
//! DLSSNR neural rendering and the two injection tiers — alongside the M1 runtime
//! subset (Proton choice, wrapper chain, diagnostics). The M1 "graphics cannot be
//! compiled" refusal is gone; a profile that asks for a field the machine cannot
//! offer is refused *by name with the capability matrix's own reason*
//! ([`EnvcError::Unavailable`]), never launched without the injection the user
//! selected. That is the plan's rule: unsupported settings are rejected at
//! compile time, never silently dropped (ADR-0004).
//!
//! [`compile`] consumes [`crate::capability`]'s verdict — it asks
//! [`capability::availability`] for every field the profile names instead of
//! deciding on its own, so the Graphics screen and the compiler cannot disagree
//! (ADR-0023's single injection slot included). A field that decision hides is
//! never emitted.
//!
//! **Mutual exclusion is a typed error with a resolve path**, not contradictory
//! env: DLSSNR, HelixSR, managed Tier B and the Proton Tier A hooks share one
//! injection slot, and a profile that names two returns
//! [`EnvcError::SlotConflict`] telling the user to keep one.
//!
//! **Options are validated against the shipped surfaces.** The upscaler and
//! frame-gen choices become `PROTON_OPTISCALER_CONFIG` settings, each checked
//! against `mods::ini::IniSurface::optiscaler()` — the shipped OptiScaler.ini key
//! set — so an unknown option is an error and `PROTON_OPTISCALER_CONFIG` can only
//! touch options that exist. Env keys from the compat database and a mod's
//! captured launch options run through the shared capture allowlist, so an
//! unsupported key is refused rather than smuggled into a launch.
//!
//! Compilation does no I/O. `session` is passed in rather than read from the
//! environment so the whole function is a pure, golden-testable mapping (the
//! caller passes `PITAGAMES_SESSION=1`, see plan §11).

use std::path::{Path, PathBuf};

use pitagames_core::profile::{
    Diagnostics, FrameGen, GamescopeOptions, Injection, InjectionDll, NeuralRendering, Profile,
    ProtonRef, Upscaler, WrapperChain, WrapperStep,
};
use pitagames_core::{
    ProfileError, ShimProton, ShimProtonInstall, flatten_wrappers, render_command,
};
use pitagames_mods::capture;
use pitagames_mods::ini::{IniSetting, IniSurface};

use crate::capability::{self, Availability, Field};
use crate::compat::CompatEntry;
use crate::gpu::GpuProfile;
use crate::proton::{ProtonInstall, ProtonKind};

/// Everything the compiler needs that is not in the profile.
#[derive(Debug, Clone, Copy)]
pub struct CompileRequest<'a> {
    pub profile: &'a Profile,
    /// The build [`ProtonRef`] resolved to; `None` when Steam decides.
    pub proton: Option<&'a ProtonInstall>,
    /// Where `PROTON_LOG_DIR` should point; `None` omits the variable.
    pub log_dir: Option<&'a Path>,
    /// `true` under the console session (`PITAGAMES_SESSION=1`): the per-game
    /// gamescope wrapper is suppressed because the session already owns one.
    pub session: bool,
}

/// The graphics inputs the M3 compiler consumes that are not in the profile.
///
/// Both come from read-only detection: [`crate::gpu`] reports the card, and the
/// game's [`CompatEntry`] is the `compat` database's curation. They are passed
/// in rather than read here, so the compiler stays a pure function and every
/// case is golden-testable with no GPU, no game and no Proton installed.
#[derive(Debug, Clone, Copy)]
pub struct GraphicsContext<'a> {
    /// The detected card, or [`GpuProfile::default`] when there is none.
    pub gpu: &'a GpuProfile,
    /// The game's curated entry, or `None` when the database does not know it.
    pub compat: Option<&'a CompatEntry>,
}

impl<'a> GraphicsContext<'a> {
    /// The graphics context for a machine and game.
    pub fn new(gpu: &'a GpuProfile, compat: Option<&'a CompatEntry>) -> Self {
        Self { gpu, compat }
    }
}

/// The compiled runtime for one launch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompiledRuntime {
    /// Environment additions, in application order (later wins).
    pub env: Vec<(String, String)>,
    /// Ordered process wrappers, outermost first.
    pub wrappers: Vec<WrapperStep>,
    /// The resolved Proton build, for the transparency UI and the shim record.
    pub proton: Option<ProtonInstall>,
    /// Ini settings the launch should apply (the DLSSNR `model_scale`, …),
    /// validated against the shipped per-mod surfaces. The mod installer writes
    /// these line-preservingly; the compiler only says what to write.
    pub ini: Vec<IniSetting>,
    /// Non-fatal notes worth showing next to the result (the plan's CachyOS /
    /// neural-rendering combination, for example).
    pub warnings: Vec<String>,
}

/// Reasons a profile cannot be compiled into a launch.
#[derive(Debug, thiserror::Error)]
pub enum EnvcError {
    #[error(transparent)]
    Profile(#[from] ProfileError),

    /// Two profile settings want the single injection slot (ADR-0023).
    ///
    /// This is the typed conflict the plan requires: a resolve path, never
    /// contradictory env such as two `PROTON_USE_OPTISCALER` values.
    #[error("{left} cannot be combined with {right}; {resolve}")]
    SlotConflict {
        left: &'static str,
        right: &'static str,
        /// The one-line action that resolves the conflict.
        resolve: &'static str,
    },

    /// The machine or the game's curation does not offer a field the profile
    /// asks for. The option is hidden, never greyed and never emitted
    /// (ADR-0004); `reason` is the capability matrix's own string.
    #[error("{} is not available: {reason}", field.label())]
    Unavailable { field: Field, reason: String },

    /// An OptiScaler option outside the shipped `OptiScaler.ini` key set, or an
    /// env key the launcher does not support. Never emitted.
    #[error("{surface} does not allow `{option}`: {reason}")]
    UnsupportedOption {
        surface: &'static str,
        option: String,
        reason: String,
    },

    /// An env key outside the launcher's supported set.
    #[error("`{key}` is not a supported environment key for {origin}")]
    UnsupportedEnvKey { key: String, origin: &'static str },
}

/// Compile the full M3 profile into a launch.
///
/// `request` carries the profile, the resolved Proton build, the log directory
/// and the console-session flag; `graphics` carries the detected GPU and the
/// game's curated entry. Every graphics field the profile names is checked
/// against [`capability::availability`] first, so a field this machine or game
/// cannot offer is refused by name rather than emitted.
pub fn compile(
    request: CompileRequest<'_>,
    graphics: GraphicsContext<'_>,
) -> Result<CompiledRuntime, EnvcError> {
    let profile = request.profile;

    // Profile validation, but promote the injection-slot conflict to one with a
    // resolve path (the plan asks for a resolve action, not a dead end).
    if let Err(error) = profile.validate() {
        return Err(match error {
            ProfileError::Conflict { left, right } => EnvcError::SlotConflict {
                left,
                right,
                resolve: RESOLVE_SLOT,
            },
            other => EnvcError::Profile(other),
        });
    }

    // The plan's one-injection-slot rule, including the combinations
    // `Profile::validate` does not enumerate (neural rendering with frame-gen).
    if let Some((left, right)) = slot_conflict(profile) {
        return Err(EnvcError::SlotConflict {
            left,
            right,
            resolve: RESOLVE_SLOT,
        });
    }

    let mut draft = Draft {
        profile,
        graphics,
        proton: request.proton,
        env: Vec::new(),
        optiscaler: Vec::new(),
        ini: Vec::new(),
        warnings: Vec::new(),
    };
    draft.compile_graphics()?;

    let mut env = draft.env;
    env.extend(diagnostics_env(&profile.diagnostics, request.log_dir));
    // The advanced overrides are appended verbatim and last: a later entry wins
    // at launch, and the transparency panel shows the user's own line.
    env.extend(profile.env.iter().map(|(k, v)| (k.clone(), v.clone())));

    Ok(CompiledRuntime {
        env,
        wrappers: wrapper_steps(&profile.wrappers, request.session),
        proton: request.proton.cloned(),
        ini: draft.ini,
        warnings: draft.warnings,
    })
}

/// Compile the runtime subset with no graphics context.
///
/// The M1 callers (the GUI's Runtime panel, the CLI) launch a machine's known
/// profile before the Graphics screen exists. They have no detected GPU here, so
/// a profile that asks for a graphics field is refused with
/// [`EnvcError::Unavailable`] and the capability matrix's "card not recognised"
/// reason — the same "never launch without the injection you selected" rule,
/// just with a different error. [`compile`] with a real [`GraphicsContext`] is
/// the full path.
pub fn compile_runtime(request: CompileRequest<'_>) -> Result<CompiledRuntime, EnvcError> {
    let gpu = GpuProfile::default();
    compile(request, GraphicsContext::new(&gpu, None))
}

/// The resolve sentence a [`EnvcError::SlotConflict`] carries.
const RESOLVE_SLOT: &str =
    "keep one injection choice and set the others off in the Graphics screen";

// ---------------------------------------------------------------------------
// The graphics draft
// ---------------------------------------------------------------------------

/// The env being built, with set-semantics so the launcher-owned keys never
/// appear twice.
struct Draft<'a> {
    profile: &'a Profile,
    graphics: GraphicsContext<'a>,
    proton: Option<&'a ProtonInstall>,
    env: Vec<(String, String)>,
    /// OptiScaler settings, collected and joined into `PROTON_OPTISCALER_CONFIG`.
    optiscaler: Vec<IniSetting>,
    ini: Vec<IniSetting>,
    warnings: Vec<String>,
}

impl Draft<'_> {
    /// Append or replace an env key (the launcher owns these, so no duplicates).
    fn set(&mut self, key: &str, value: &str) {
        if let Some(slot) = self.env.iter_mut().find(|(existing, _)| existing == key) {
            slot.1 = value.to_string();
            return;
        }
        self.env.push((key.to_string(), value.to_string()));
    }

    /// Refuse a field the machine or game cannot offer, with the matrix reason.
    fn require(&self, field: Field) -> Result<(), EnvcError> {
        match capability::availability(
            field,
            self.profile,
            self.graphics.gpu,
            self.graphics.compat,
            self.proton,
        ) {
            Availability::Available => Ok(()),
            Availability::Hidden { reason } => Err(EnvcError::Unavailable { field, reason }),
        }
    }

    /// Add an OptiScaler setting, validated against the shipped ini surface.
    fn add_optiscaler(&mut self, key: &str, value: &str) -> Result<(), EnvcError> {
        let setting = IniSetting::parse(&format!("{key}={value}")).map_err(|error| {
            EnvcError::UnsupportedOption {
                surface: OPTISCALER_INI,
                option: key.to_string(),
                reason: error.to_string(),
            }
        })?;
        IniSurface::optiscaler()
            .validate(&setting)
            .map_err(|error| EnvcError::UnsupportedOption {
                surface: OPTISCALER_INI,
                option: key.to_string(),
                reason: error.to_string(),
            })?;
        // Later wins: a profile override beats the value the choice derived.
        if let Some(slot) = self
            .optiscaler
            .iter_mut()
            .find(|s| s.section == setting.section && s.key == setting.key)
        {
            slot.value = setting.value;
        } else {
            self.optiscaler.push(setting);
        }
        Ok(())
    }

    /// Join every OptiScaler setting into the one config variable.
    fn flush_optiscaler(&mut self) {
        if self.optiscaler.is_empty() {
            return;
        }
        let joined = self
            .optiscaler
            .iter()
            .map(|s| format!("{}.{}={}", s.section, s.key, s.value))
            .collect::<Vec<_>>()
            .join(";");
        self.set("PROTON_OPTISCALER_CONFIG", &joined);
    }

    /// Compile the plan §6 table for this profile, field by field.
    fn compile_graphics(&mut self) -> Result<(), EnvcError> {
        let upscaler = Field::for_upscaler(&self.profile.upscaler)
            .expect("every upscaler choice maps to a field");
        self.require(upscaler)?;
        self.emit_upscaler()?;

        if let Some(field) = Field::for_frame_gen(self.profile.frame_gen) {
            self.require(field)?;
            self.emit_frame_gen()?;
        }

        if self.profile.neural_rendering.is_enabled() {
            self.require(Field::NeuralRendering)?;
            self.emit_neural_rendering()?;
        }

        if let Some(field) = Field::for_injection(self.profile.injection) {
            self.require(field)?;
            self.emit_injection();
        }

        // A curated game may set its own allowlisted runtime env.
        self.emit_compat_env()?;

        // The profile's own OptiScaler overrides ride the same config variable,
        // validated against the same surface.
        let overrides: Vec<(String, String)> = self
            .profile
            .ini_overrides
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, value) in overrides {
            self.add_optiscaler(&key, &value)?;
        }
        self.flush_optiscaler();

        Ok(())
    }

    /// The plan §6 upscaler row.
    fn emit_upscaler(&mut self) -> Result<(), EnvcError> {
        match self.profile.upscaler {
            Upscaler::Off | Upscaler::Native | Upscaler::SystemFsr => {}
            Upscaler::Fsr4Upgrade { ref version } => {
                self.set("PROTON_FSR4_UPGRADE", version.as_deref().unwrap_or("1"));
            }
            Upscaler::Fsr4Rdna3 { ref version } => {
                self.set(
                    "PROTON_FSR4_RDNA3_UPGRADE",
                    version.as_deref().unwrap_or("1"),
                );
            }
            Upscaler::DlssToFsr4 => {
                self.set("PROTON_USE_OPTISCALER", "1");
                self.set("PROTON_FSR4_UPGRADE", "1");
                self.add_optiscaler("Upscalers.Dx12Upscaler", "fsr4")?;
            }
            Upscaler::DlssToFsr31 => {
                self.set("PROTON_USE_OPTISCALER", "1");
                self.add_optiscaler("Upscalers.Dx12Upscaler", "fsr31")?;
            }
            Upscaler::DlssToXess => {
                self.set("PROTON_USE_OPTISCALER", "1");
                self.set("PROTON_XESS_UPGRADE", "1");
                self.add_optiscaler("Upscalers.Dx12Upscaler", "xess")?;
            }
            Upscaler::XessUpgrade => self.set("PROTON_XESS_UPGRADE", "1"),
            Upscaler::Ffx3Upgrade => self.set("PROTON_FFX3_UPGRADE", "1"),
            Upscaler::Ffx4Upgrade => self.set("PROTON_FFX4_UPGRADE", "1"),
            // HelixSR ships its own OptiScaler copy; the upscaler itself is
            // chosen in its in-game menu, so only the OptiScaler flag is needed.
            Upscaler::HlxDlss => {
                self.set("PROTON_USE_OPTISCALER", "1");
                self.add_optiscaler("Upscalers.Dx12Upscaler", "dlss")?;
            }
            Upscaler::HlxFsr4 => {
                self.set("PROTON_USE_OPTISCALER", "1");
                self.add_optiscaler("Upscalers.Dx12Upscaler", "fsr4")?;
            }
            // The drop-in replaces the game's FSR 3.1 DLL; no env, no menu.
            Upscaler::HlxFsr4DropIn => {}
        }
        Ok(())
    }

    /// The plan §6 frame-generation row.
    fn emit_frame_gen(&mut self) -> Result<(), EnvcError> {
        match self.profile.frame_gen {
            FrameGen::Off => {}
            FrameGen::Fsr4Mlfg => self.set("PROTON_MLFG_UPGRADE", "1"),
            FrameGen::Fsr3 => {
                self.set("PROTON_USE_OPTISCALER", "1");
                self.add_optiscaler("FrameGen.Enabled", "true")?;
                self.add_optiscaler("FrameGen.FGType", "fsr3")?;
            }
        }
        Ok(())
    }

    /// The plan §6 neural-rendering row.
    fn emit_neural_rendering(&mut self) -> Result<(), EnvcError> {
        // DLSSNR ships its own OptiScaler. Force Proton's off so the two copies
        // can never meet (ADR-0023), whichever Proton is selected.
        self.set("PROTON_USE_OPTISCALER", "0");

        let NeuralRendering::DlssNrAmd {
            model_scale,
            captured_env,
            ..
        } = &self.profile.neural_rendering
        else {
            return Ok(());
        };

        // The installer's printed launch options, already allowlisted at capture
        // time; re-checked here so a hand-edited profile cannot smuggle a key.
        let captured: Vec<(String, String)> = captured_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, value) in captured {
            if !capture::is_allowed_key(&key) {
                return Err(EnvcError::UnsupportedEnvKey {
                    key,
                    origin: "a mod's captured launch options",
                });
            }
            self.set(&key, &value);
        }

        // The network resolution is a `dlssnr-amd.ini` setting (the main cost
        // lever on RDNA 3), validated against the shipped DLSSNR surface.
        let setting = IniSetting::new("Network", "model_scale", model_scale.to_string());
        IniSurface::dlssnr()
            .validate(&setting)
            .map_err(|error| EnvcError::UnsupportedOption {
                surface: DLSSNR_INI,
                option: format!("Network.model_scale={model_scale}"),
                reason: error.to_string(),
            })?;
        self.ini.push(setting);

        if self
            .proton
            .is_some_and(|install| install.kind == ProtonKind::CachyOs)
        {
            self.warnings.push(
                "neural rendering is tested with GE-Proton or Valve Proton; \
                 Proton-CachyOS runs with its OptiScaler injection disabled"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// The plan §6 injection-tier row.
    fn emit_injection(&mut self) {
        match self.profile.injection {
            // Tier A is Proton's own hook, emitted by the upscaler/frame-gen
            // rows (or nothing at all for an env-only injection).
            Injection::Auto | Injection::Off | Injection::EnvOnly | Injection::HelixSr => {}
            Injection::Manual => {
                // Tier B deploys the files (in `mods::tierb`); the compiler only
                // contributes the override that makes Wine load them.
                let dll = injection_dll_name(self.profile.injection_dll);
                self.set("WINEDLLOVERRIDES", &format!("{dll}=n,b"));
            }
            // Emitted by the neural-rendering row.
            Injection::DlssNr => {}
        }
    }

    /// Per-game environment from the curated compat entry, key-checked.
    fn emit_compat_env(&mut self) -> Result<(), EnvcError> {
        let Some(entry) = self.graphics.compat else {
            return Ok(());
        };
        let entries: Vec<(String, String)> = entry
            .runtime
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, value) in entries {
            if !capture::is_allowed_key(&key) {
                return Err(EnvcError::UnsupportedEnvKey {
                    key,
                    origin: "the compatibility database",
                });
            }
            self.set(&key, &value);
        }
        Ok(())
    }
}

/// The shipped `OptiScaler.ini` key set, named in errors.
const OPTISCALER_INI: &str = "OptiScaler.ini";
/// The shipped DLSSNR ini surface, named in errors.
const DLSSNR_INI: &str = "dlssnr-amd.ini";

/// The `WINEDLLOVERRIDES` key for a Tier B injection's DLL choice.
fn injection_dll_name(dll: InjectionDll) -> &'static str {
    match dll {
        InjectionDll::Dxgi => "dxgi",
        InjectionDll::D3d12 => "d3d12",
        InjectionDll::Dbghelp => "dbghelp",
        InjectionDll::Winmm => "winmm",
        InjectionDll::Version => "version",
    }
}

// ---------------------------------------------------------------------------
// The one injection slot (ADR-0023)
// ---------------------------------------------------------------------------

/// The four mechanisms that contend for the single injection slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Proton's own env hooks (FSR/FFX/XeSS upgrades, OptiScaler, MLFG).
    TierA,
    /// The launcher's managed OptiScaler files.
    TierB,
    /// HelixSR, which ships its own OptiScaler.
    HelixSr,
    /// DLSSNR-AMD, which ships its own OptiScaler too.
    DlssNr,
}

/// The slot mechanisms this profile uses, in a stable order.
fn slot_groups(profile: &Profile) -> Vec<(&'static str, Slot)> {
    let mut groups = Vec::new();

    let tier_a = profile.injection == Injection::EnvOnly
        || matches!(
            profile.upscaler,
            Upscaler::Fsr4Upgrade { .. }
                | Upscaler::Fsr4Rdna3 { .. }
                | Upscaler::XessUpgrade
                | Upscaler::Ffx3Upgrade
                | Upscaler::Ffx4Upgrade
                | Upscaler::DlssToFsr4
                | Upscaler::DlssToFsr31
                | Upscaler::DlssToXess
        )
        || profile.frame_gen != FrameGen::Off;
    if tier_a {
        groups.push(("Tier A (Proton env injection)", Slot::TierA));
    }
    if profile.injection == Injection::Manual {
        groups.push(("Tier B (managed OptiScaler injection)", Slot::TierB));
    }
    if matches!(
        profile.upscaler,
        Upscaler::HlxDlss | Upscaler::HlxFsr4 | Upscaler::HlxFsr4DropIn
    ) || profile.injection == Injection::HelixSr
    {
        groups.push(("HelixSR", Slot::HelixSr));
    }
    if profile.neural_rendering.is_enabled() || profile.injection == Injection::DlssNr {
        groups.push(("neural rendering", Slot::DlssNr));
    }

    groups
}

/// The first two mechanisms that cannot share the slot, if any.
fn slot_conflict(profile: &Profile) -> Option<(&'static str, &'static str)> {
    let groups = slot_groups(profile);
    if groups.len() < 2 {
        return None;
    }
    Some((groups[0].0, groups[1].0))
}

// ---------------------------------------------------------------------------
// Wrappers, diagnostics, Proton record (unchanged from M1)
// ---------------------------------------------------------------------------

/// Expand a [`WrapperChain`] into the ordered steps a launch execs.
///
/// The GUI's wrapper editor previews through this function, so ordering and the
/// console-session gamescope suppression cannot drift from the launch path.
#[must_use]
pub fn wrapper_steps(chain: &WrapperChain, session: bool) -> Vec<WrapperStep> {
    let mut wrappers = Vec::new();

    if let Some(gamescope) = &chain.gamescope {
        // Nested gamescope is a bug: the console session owns the compositor.
        if !session {
            wrappers.push(gamescope_step(gamescope));
        }
    }
    if chain.gamemode {
        wrappers.push(program("gamemoderun"));
    }
    if chain.mangohud {
        wrappers.push(program("mangohud"));
    }
    wrappers.extend(chain.custom.iter().cloned());
    wrappers
}

/// Render the compiled wrapper chain in front of `command` as a copyable line.
///
/// Used by the wrapper editor's live preview. `command` is usually `%command%`:
/// the GUI does not know Steam's game command until launch.
#[must_use]
pub fn render_wrapper_chain(chain: &WrapperChain, session: bool, command: &str) -> String {
    let steps = wrapper_steps(chain, session);
    let (program, args) = flatten_wrappers(&steps, command, &[]);
    render_command(&program, &args)
}

fn diagnostics_env(diagnostics: &Diagnostics, log_dir: Option<&Path>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if diagnostics.dxvk_hud {
        env.push(("DXVK_HUD".to_string(), "fps".to_string()));
    }
    if diagnostics.mangohud {
        env.push(("MANGOHUD".to_string(), "1".to_string()));
    }
    if diagnostics.dxvk_log {
        // DXVK writes `<exe>_d3d11.log` etc into `DXVK_LOG_PATH`; without a log
        // directory it logs to stderr / the Proton log, so only the level is set.
        env.push(("DXVK_LOG_LEVEL".to_string(), "debug".to_string()));
        if let Some(dir) = log_dir {
            env.push((
                "DXVK_LOG_PATH".to_string(),
                dir.to_string_lossy().into_owned(),
            ));
        }
    }
    if diagnostics.proton_log {
        env.push(("PROTON_LOG".to_string(), "1".to_string()));
        if let Some(dir) = log_dir {
            env.push((
                "PROTON_LOG_DIR".to_string(),
                dir.to_string_lossy().into_owned(),
            ));
        }
    }
    if diagnostics.fsr4_indicator {
        env.push(("PROTON_FSR4_INDICATOR".to_string(), "1".to_string()));
    }
    if diagnostics.dlss_indicator {
        env.push(("PROTON_DLSS_INDICATOR".to_string(), "1".to_string()));
    }
    env
}

fn gamescope_step(options: &GamescopeOptions) -> WrapperStep {
    let mut args = Vec::new();
    if let Some(width) = options.width {
        args.push("-w".to_string());
        args.push(width.to_string());
    }
    if let Some(height) = options.height {
        args.push("-h".to_string());
        args.push(height.to_string());
    }
    if let Some(refresh) = options.refresh_hz {
        args.push("-r".to_string());
        args.push(refresh.to_string());
    }
    if options.fullscreen {
        args.push("--fullscreen".to_string());
    }
    args.extend(options.extra_args.iter().cloned());
    // gamescope needs an explicit separator before the wrapped command.
    args.push("--".to_string());

    WrapperStep {
        program: PathBuf::from("gamescope"),
        args,
    }
}

fn program(name: &str) -> WrapperStep {
    WrapperStep {
        program: PathBuf::from(name),
        args: Vec::new(),
    }
}

/// Flatten a [`ProtonRef`] and the build it resolved to into the shim's record.
pub fn shim_proton(reference: &ProtonRef, install: Option<&ProtonInstall>) -> ShimProton {
    ShimProton {
        reference: reference.clone(),
        install: install.map(|install| ShimProtonInstall {
            name: install.name.clone(),
            path: install.path.clone(),
            kind: install.kind.label().to_string(),
            version: install.distinct_version().map(str::to_string),
            source: install.source.label().to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{GameId, Store};
    use std::path::PathBuf;

    fn game() -> GameId {
        GameId::new(Store::Steam, "620")
    }

    fn install() -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
            name: "proton-cachyos".to_string(),
            kind: crate::proton::ProtonKind::CachyOs,
            version: Some("proton-cachyos 10.0-20251101".to_string()),
            source: crate::proton::ProtonSource::SystemCompatibilityTools,
        }
    }

    fn request<'a>(profile: &'a Profile, proton: Option<&'a ProtonInstall>) -> CompileRequest<'a> {
        CompileRequest {
            profile,
            proton,
            log_dir: Some(Path::new("/data/pitagames/logs")),
            session: false,
        }
    }

    #[test]
    fn the_default_profile_compiles_to_nothing() {
        let profile = Profile::new(game(), "Default");
        let compiled = compile_runtime(request(&profile, None)).unwrap();
        assert!(compiled.env.is_empty());
        assert!(compiled.wrappers.is_empty());
        assert!(compiled.proton.is_none());
        assert!(compiled.ini.is_empty());
        assert!(compiled.warnings.is_empty());
    }

    #[test]
    fn diagnostics_become_the_documented_env() {
        let mut profile = Profile::new(game(), "Debug");
        profile.diagnostics = Diagnostics {
            dxvk_hud: true,
            mangohud: true,
            proton_log: true,
            fsr4_indicator: true,
            dlss_indicator: true,
            ..Diagnostics::default()
        };

        let compiled = compile_runtime(request(&profile, None)).unwrap();
        assert_eq!(
            compiled.env,
            vec![
                ("DXVK_HUD".to_string(), "fps".to_string()),
                ("MANGOHUD".to_string(), "1".to_string()),
                ("PROTON_LOG".to_string(), "1".to_string()),
                (
                    "PROTON_LOG_DIR".to_string(),
                    "/data/pitagames/logs".to_string()
                ),
                ("PROTON_FSR4_INDICATOR".to_string(), "1".to_string()),
                ("PROTON_DLSS_INDICATOR".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn a_missing_log_dir_omits_the_variable() {
        let mut profile = Profile::new(game(), "Debug");
        profile.diagnostics.proton_log = true;
        let request = CompileRequest {
            profile: &profile,
            proton: None,
            log_dir: None,
            session: false,
        };
        let compiled = compile_runtime(request).unwrap();
        assert_eq!(
            compiled.env,
            vec![("PROTON_LOG".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn dxvk_logging_sets_the_level_and_points_at_the_log_dir() {
        let mut profile = Profile::new(game(), "Debug");
        profile.diagnostics.dxvk_log = true;

        let compiled = compile_runtime(request(&profile, None)).unwrap();
        assert_eq!(
            compiled.env,
            vec![
                ("DXVK_LOG_LEVEL".to_string(), "debug".to_string()),
                (
                    "DXVK_LOG_PATH".to_string(),
                    "/data/pitagames/logs".to_string()
                ),
            ]
        );

        // Without a log directory only the level is set: DXVK then logs to
        // stderr (which Proton captures) instead of a file we cannot name.
        let request = CompileRequest {
            profile: &profile,
            proton: None,
            log_dir: None,
            session: false,
        };
        assert_eq!(
            compile_runtime(request).unwrap().env,
            vec![("DXVK_LOG_LEVEL".to_string(), "debug".to_string())]
        );
    }

    #[test]
    fn advanced_env_overrides_are_appended_last() {
        let mut profile = Profile::new(game(), "Advanced");
        profile.diagnostics.dxvk_hud = true;
        profile.env.insert("DXVK_HUD".into(), "full".into());
        profile.env.insert("DXVK_HDR".into(), "1".into());

        let compiled = compile_runtime(request(&profile, None)).unwrap();
        // `env` is a BTreeMap, so the advanced keys are appended in sorted
        // order; `DXVK_HUD` appears twice and the later value wins at launch.
        assert_eq!(
            compiled.env,
            vec![
                ("DXVK_HUD".to_string(), "fps".to_string()),
                ("DXVK_HDR".to_string(), "1".to_string()),
                ("DXVK_HUD".to_string(), "full".to_string()),
            ]
        );
    }

    #[test]
    fn wrappers_are_ordered_outermost_first() {
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

        let compiled = compile_runtime(request(&profile, None)).unwrap();
        assert_eq!(
            compiled.wrappers,
            vec![
                WrapperStep {
                    program: PathBuf::from("gamescope"),
                    args: vec![
                        "-w".into(),
                        "2560".into(),
                        "-h".into(),
                        "1440".into(),
                        "-r".into(),
                        "165".into(),
                        "--fullscreen".into(),
                        "--hdr-enabled".into(),
                        "--".into(),
                    ],
                },
                WrapperStep {
                    program: PathBuf::from("gamemoderun"),
                    args: vec![],
                },
                WrapperStep {
                    program: PathBuf::from("mangohud"),
                    args: vec![],
                },
                WrapperStep {
                    program: PathBuf::from("/usr/bin/mangohud"),
                    args: vec!["--dls".into()],
                },
            ]
        );
    }

    #[test]
    fn the_wrapper_preview_matches_the_compiled_chain() {
        let chain = WrapperChain {
            gamemode: true,
            mangohud: true,
            gamescope: Some(GamescopeOptions {
                width: Some(2560),
                height: Some(1440),
                refresh_hz: None,
                fullscreen: true,
                extra_args: vec!["--hdr-enabled".into()],
            }),
            custom: vec![WrapperStep {
                program: PathBuf::from("prism-run"),
                args: vec!["--prefer-wayland".into(), "two words".into()],
            }],
        };

        assert_eq!(
            render_wrapper_chain(&chain, false, "%command%"),
            "gamescope -w 2560 -h 1440 --fullscreen --hdr-enabled -- gamemoderun mangohud \
             prism-run --prefer-wayland 'two words' %command%"
        );
        assert_eq!(
            render_wrapper_chain(&chain, true, "%command%"),
            "gamemoderun mangohud prism-run --prefer-wayland 'two words' %command%"
        );
        assert_eq!(
            render_wrapper_chain(&WrapperChain::default(), false, "%command%"),
            "%command%"
        );
    }

    #[test]
    fn the_console_session_suppresses_gamescope_but_keeps_the_rest() {
        let mut profile = Profile::new(game(), "Console");
        profile.wrappers = WrapperChain {
            gamemode: true,
            gamescope: Some(GamescopeOptions::default()),
            ..WrapperChain::default()
        };

        let compiled = compile_runtime(CompileRequest {
            session: true,
            ..request(&profile, None)
        })
        .unwrap();
        assert_eq!(
            compiled.wrappers,
            vec![WrapperStep {
                program: PathBuf::from("gamemoderun"),
                args: vec![],
            }]
        );
    }

    #[test]
    fn proton_selection_is_recorded_but_not_turned_into_env_yet() {
        let profile = Profile::new(game(), "Proton");
        let install = install();
        let compiled = compile_runtime(request(&profile, Some(&install))).unwrap();
        assert_eq!(compiled.proton.as_ref(), Some(&install));
        assert!(
            compiled.env.is_empty(),
            "PROTONPATH is an umu/non-Steam concern"
        );
    }

    #[test]
    fn a_graphics_choice_on_an_unknown_card_is_refused_by_name() {
        // `compile_runtime` carries no GPU, so the card is unrecognised and the
        // old "compiled in M3" refusal is gone: the field is refused with the
        // capability matrix's own reason instead.
        let mut profile = Profile::new(game(), "FSR 4");
        profile.upscaler = Upscaler::Fsr4Upgrade {
            version: Some("4.0.3".into()),
        };
        let error = compile_runtime(request(&profile, None)).unwrap_err();
        assert!(
            matches!(
                error,
                EnvcError::Unavailable { field: Field::Fsr4Upgrade, ref reason }
                    if reason == capability::reason::GPU_UNKNOWN
            ),
            "{error}"
        );
    }

    #[test]
    fn a_profile_with_two_injection_owners_is_a_typed_conflict() {
        // Neural rendering plus FSR 3 frame generation: both want OptiScaler,
        // and `Profile::validate` does not enumerate this pair.
        let mut profile = Profile::new(game(), "Conflicted");
        profile.neural_rendering = nr();
        profile.frame_gen = FrameGen::Fsr3;
        let error = compile_runtime(request(&profile, None)).unwrap_err();
        assert!(
            matches!(
                error,
                EnvcError::SlotConflict { left, right, resolve }
                    if left == "Tier A (Proton env injection)"
                        && right == "neural rendering"
                        && !resolve.is_empty()
            ),
            "{error}"
        );
    }

    #[test]
    fn a_profile_validate_conflict_gains_a_resolve_path() {
        let mut profile = Profile::new(game(), "Conflicted");
        profile.neural_rendering = nr();
        profile.injection = Injection::Manual;
        let error = compile_runtime(request(&profile, None)).unwrap_err();
        assert!(
            matches!(error, EnvcError::SlotConflict { resolve, .. } if resolve == RESOLVE_SLOT),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_compat_env_key_is_refused() {
        use crate::compat::CompatEntry;
        let entry = CompatEntry::from_toml(
            "[game]\nstore = \"steam\"\nstore_id = \"620\"\ntitle = \"x\"\n\n\
             [runtime]\nenv = { SECRET_TOKEN = \"x\" }\n",
        )
        .unwrap();
        let profile = Profile::new(game(), "Game");
        let gpu = GpuProfile::default();
        let error = compile(
            request(&profile, None),
            GraphicsContext::new(&gpu, Some(&entry)),
        )
        .unwrap_err();
        assert!(
            matches!(error, EnvcError::UnsupportedEnvKey { ref key, .. } if key == "SECRET_TOKEN"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_optiscaler_option_is_refused() {
        // An ini override for an option the shipped OptiScaler.ini does not have
        // is an error, never emitted into `PROTON_OPTISCALER_CONFIG`.
        let mut profile = Profile::new(game(), "Override");
        profile
            .ini_overrides
            .insert("Secrets.Token".to_string(), "x".to_string());
        let error = compile_runtime(request(&profile, None)).unwrap_err();
        assert!(
            matches!(error, EnvcError::UnsupportedOption { ref option, .. } if option == "Secrets.Token"),
            "{error}"
        );
    }

    #[test]
    fn invalid_profiles_fail_validation_before_compiling() {
        let profile = Profile::new(game(), "   ");
        assert!(matches!(
            compile_runtime(request(&profile, None)),
            Err(EnvcError::Profile(ProfileError::EmptyName))
        ));
    }

    #[test]
    fn shim_proton_flattens_the_resolved_build() {
        let install = install();
        let record = shim_proton(&ProtonRef::CachyOs, Some(&install));
        assert_eq!(record.reference, ProtonRef::CachyOs);
        let record = record.install.unwrap();
        assert_eq!(record.name, "proton-cachyos");
        assert_eq!(record.kind, "proton-cachyos");
        assert_eq!(record.source, "system");
        assert_eq!(
            record.version.as_deref(),
            Some("proton-cachyos 10.0-20251101")
        );

        // Steam decides: no install recorded.
        let record = shim_proton(&ProtonRef::SteamDefault, None);
        assert!(record.install.is_none());
    }

    fn nr() -> NeuralRendering {
        use pitagames_core::profile::{ModelSource, NrBackend, NrModel, NrProcessing, NrRoute};
        NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.8".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 100,
            processing: NrProcessing::RenoDx,
            pkg_arch: pitagames_core::profile::PkgArch::X86_64,
            captured_env: Default::default(),
            model_source: ModelSource::CachedInPackage,
        }
    }
}
