//! The launch compiler.
//!
//! A [`Profile`] is pure data. `platform::envc` turns it into the concrete
//! environment and wrapper chain a launch needs, together with the Proton build
//! it resolved to. The shim, the GUI and the CLI all consume the result through
//! [`ShimProfile`](pitagames_core::ShimProfile), so there is one compiler, not
//! three.
//!
//! **M1 scope — the runtime subset.** This build compiles the Proton choice, the
//! wrapper chain and the diagnostic environment. The graphics table (FSR 4,
//! DLSS→FSR, XeSS, HelixSR, DLSSNR `captured_env`, Tier A/B injection) arrives in
//! M3; a profile that asks for any of it is refused by name rather than launched
//! without the injection the user selected. That is the plan's rule: unsupported
//! settings are rejected at compile time, never silently dropped.
//!
//! Compilation does no I/O. `session` is passed in rather than read from the
//! environment so the whole function is a pure, golden-testable mapping (the
//! caller passes `PITAGAMES_SESSION=1`, see plan §11).

use std::path::{Path, PathBuf};

use pitagames_core::profile::{
    Diagnostics, FrameGen, GamescopeOptions, Injection, Profile, ProtonRef, Upscaler, WrapperChain,
    WrapperStep,
};
use pitagames_core::{
    ProfileError, ShimProton, ShimProtonInstall, flatten_wrappers, render_command,
};

use crate::proton::ProtonInstall;

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

/// The compiled runtime for one launch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompiledRuntime {
    /// Environment additions, in application order (later wins).
    pub env: Vec<(String, String)>,
    /// Ordered process wrappers, outermost first.
    pub wrappers: Vec<WrapperStep>,
    /// The resolved Proton build, for the transparency UI and the shim record.
    pub proton: Option<ProtonInstall>,
}

/// Reasons a profile cannot be compiled into a launch.
#[derive(Debug, thiserror::Error)]
pub enum EnvcError {
    #[error(transparent)]
    Profile(#[from] ProfileError),

    #[error(
        "{feature} settings are compiled in M3; this build launches the runtime only \
         (Proton, wrappers, diagnostics)"
    )]
    GraphicsDeferred { feature: &'static str },
}

/// Compile the runtime subset of a profile.
pub fn compile_runtime(request: CompileRequest<'_>) -> Result<CompiledRuntime, EnvcError> {
    let profile = request.profile;
    profile.validate()?;

    if let Some(feature) = deferred_graphics(profile) {
        return Err(EnvcError::GraphicsDeferred { feature });
    }

    let mut env = diagnostics_env(&profile.diagnostics, request.log_dir);
    env.extend(profile.env.iter().map(|(k, v)| (k.clone(), v.clone())));

    Ok(CompiledRuntime {
        env,
        wrappers: wrapper_steps(&profile.wrappers, request.session),
        proton: request.proton.cloned(),
    })
}

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

/// The first graphics field that M1 cannot compile, if any.
fn deferred_graphics(profile: &Profile) -> Option<&'static str> {
    if !matches!(
        profile.upscaler,
        Upscaler::Native | Upscaler::Off | Upscaler::SystemFsr
    ) {
        return Some("upscaler");
    }
    if profile.frame_gen != FrameGen::Off {
        return Some("frame generation");
    }
    if profile.neural_rendering.is_enabled() {
        return Some("neural rendering");
    }
    match profile.injection {
        Injection::Auto | Injection::Off => None,
        Injection::EnvOnly => Some("Tier A injection"),
        Injection::Manual => Some("manual OptiScaler injection"),
        Injection::HelixSr => Some("HelixSR injection"),
        Injection::DlssNr => Some("DLSSNR injection"),
    }
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
    fn graphics_settings_are_refused_by_name_not_dropped() {
        use pitagames_core::{NrBackend, NrModel, NrProcessing, NrRoute, PkgArch};

        let mut profile = Profile::new(game(), "FSR 4");
        profile.upscaler = Upscaler::Fsr4Upgrade {
            version: Some("4.0.3".into()),
        };
        let error = compile_runtime(request(&profile, None)).unwrap_err();
        assert!(
            matches!(error, EnvcError::GraphicsDeferred { feature } if feature == "upscaler"),
            "{error}"
        );

        let mut profile = Profile::new(game(), "FG");
        profile.frame_gen = FrameGen::Fsr3;
        assert!(matches!(
            compile_runtime(request(&profile, None)),
            Err(EnvcError::GraphicsDeferred {
                feature: "frame generation"
            })
        ));

        let mut profile = Profile::new(game(), "Inject");
        profile.injection = Injection::Manual;
        assert!(matches!(
            compile_runtime(request(&profile, None)),
            Err(EnvcError::GraphicsDeferred {
                feature: "manual OptiScaler injection"
            })
        ));

        let mut profile = Profile::new(game(), "NR");
        profile.neural_rendering = pitagames_core::NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.8".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 100,
            processing: NrProcessing::RenoDx,
            pkg_arch: PkgArch::X86_64,
            captured_env: Default::default(),
            model_source: pitagames_core::ModelSource::CachedInPackage,
        };
        assert!(matches!(
            compile_runtime(request(&profile, None)),
            Err(EnvcError::GraphicsDeferred {
                feature: "neural rendering"
            })
        ));
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
}
