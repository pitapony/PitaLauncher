//! The per-game runtime: the GUI's link to `platform` and `stores`.
//!
//! Everything the detail page does that is not rendering lives here, so it is
//! testable without a display:
//!
//! - discover the Proton builds on the machine;
//! - compile a profile and write its `profiles/steam-<appid>.json`;
//! - read and write the game's Steam launch options so Steam runs the shim.
//!
//! The write path is [`pitagames_stores::steam_config`], the same guarded,
//! backup-taking, atomic writer the CLI diagnostic uses. This module only
//! resolves the Steam root and turns errors into display-ready strings.

use std::path::{Path, PathBuf};

use pitagames_core::profile::{Diagnostics, GamescopeOptions, WrapperChain, WrapperStep};
use pitagames_core::{
    Game, LaunchSpec, Profile, ProtonRef, SHIM_PROGRAM, ShimProfile, Store, flatten_wrappers,
    render_shell_command, save_shim_profile, shell_quote, shim_launch_options, tokenize_command,
};
use pitagames_platform::envc::{self, CompileRequest, CompiledRuntime, GraphicsContext};
use pitagames_platform::gpu::GpuProfile;
use pitagames_platform::launch::{self as handoff, Prepared};
use pitagames_platform::proton::{self, DiscoveryReport, ProtonInstall, ProtonSource};
use pitagames_stores::compat_tool::CompatTool;
use pitagames_stores::steam::user_configs;
use pitagames_stores::steam_config::{
    self, CompatRestoreReport, CompatWriteReport, RestoreReport, SteamProcessState, WriteOptions,
    WriteReport,
};
use pitagames_stores::sync::Backends;

/// Every Proton build discovery found, best first.
pub fn discover_proton() -> DiscoveryReport {
    proton::discover()
}

/// `true` under the console session, where the per-game gamescope wrapper is
/// suppressed (nested gamescope is a bug).
pub fn session_mode() -> bool {
    std::env::var_os("PITAGAMES_SESSION").is_some_and(|value| !value.is_empty())
}

/// The editable form of a [`WrapperChain`].
///
/// The GUI cannot put `Option<u32>` or an argv vector straight into a text
/// input, so the editor keeps strings and converts once, on save. Conversion is
/// the validation boundary: a half-typed number or an unbalanced quote is
/// reported to the user instead of reaching the profile (and then the shim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperDraft {
    pub gamemode: bool,
    pub mangohud: bool,
    pub gamescope: bool,
    pub width: String,
    pub height: String,
    pub refresh_hz: String,
    pub fullscreen: bool,
    pub extra_args: String,
    pub custom: Vec<WrapperDraftStep>,
}

/// One custom wrapper row, kept as the user typed it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WrapperDraftStep {
    pub program: String,
    pub args: String,
}

impl Default for WrapperDraft {
    fn default() -> Self {
        Self {
            gamemode: false,
            mangohud: false,
            gamescope: false,
            width: String::new(),
            height: String::new(),
            refresh_hz: String::new(),
            // Enabling gamescope from an empty chain should start fullscreen.
            fullscreen: true,
            extra_args: String::new(),
            custom: Vec::new(),
        }
    }
}

impl WrapperDraft {
    /// Load the editor from a stored profile.
    pub fn from_chain(chain: &WrapperChain) -> Self {
        let gamescope = chain.gamescope.as_ref();
        Self {
            gamemode: chain.gamemode,
            mangohud: chain.mangohud,
            gamescope: gamescope.is_some(),
            width: optional_number(gamescope.and_then(|options| options.width)),
            height: optional_number(gamescope.and_then(|options| options.height)),
            refresh_hz: optional_number(gamescope.and_then(|options| options.refresh_hz)),
            fullscreen: gamescope.is_none_or(|options| options.fullscreen),
            extra_args: gamescope
                .map(|options| render_args(&options.extra_args))
                .unwrap_or_default(),
            custom: chain
                .custom
                .iter()
                .map(|step| WrapperDraftStep {
                    program: step.program.to_string_lossy().into_owned(),
                    args: render_args(&step.args),
                })
                .collect(),
        }
    }

    /// Convert the editor back into a validated [`WrapperChain`].
    pub fn to_chain(&self) -> Result<WrapperChain, String> {
        let gamescope = if self.gamescope {
            Some(GamescopeOptions {
                width: parse_dimension(&self.width, "Gamescope width")?,
                height: parse_dimension(&self.height, "Gamescope height")?,
                refresh_hz: parse_dimension(&self.refresh_hz, "Gamescope refresh rate")?,
                fullscreen: self.fullscreen,
                extra_args: parse_args(&self.extra_args, "Gamescope extra args")?,
            })
        } else {
            None
        };

        let mut custom = Vec::with_capacity(self.custom.len());
        for (index, step) in self.custom.iter().enumerate() {
            let program = step.program.trim();
            if program.is_empty() {
                return Err(format!("Custom wrapper #{} needs a program.", index + 1));
            }
            custom.push(WrapperStep {
                program: PathBuf::from(program),
                args: parse_args(&step.args, &format!("Custom wrapper #{} args", index + 1))?,
            });
        }

        Ok(WrapperChain {
            gamemode: self.gamemode,
            mangohud: self.mangohud,
            gamescope,
            custom,
        })
    }

    /// Append a custom row, reporting a malformed program or argument list now
    /// rather than at save time.
    pub fn add_step(&mut self, program: &str, args: &str) -> Result<(), String> {
        let program = program.trim();
        if program.is_empty() {
            return Err("A custom wrapper needs a program.".to_string());
        }
        parse_args(args, "Custom wrapper args")?;
        self.custom.push(WrapperDraftStep {
            program: program.to_string(),
            args: args.trim().to_string(),
        });
        Ok(())
    }

    /// Write the chain into a profile and run the profile's own validation.
    ///
    /// Validation happens on a copy, so a refused draft never leaves the
    /// profile half-edited.
    pub fn apply_to(&self, profile: &mut Profile) -> Result<(), String> {
        let mut next = profile.clone();
        next.wrappers = self.to_chain()?;
        next.validate().map_err(|error| error.to_string())?;
        *profile = next;
        Ok(())
    }

    /// The chain flattened in front of `%command%`, for the editor preview.
    pub fn preview(&self) -> Result<String, String> {
        self.preview_in(session_mode())
    }

    fn preview_in(&self, session: bool) -> Result<String, String> {
        let chain = self.to_chain()?;
        Ok(envc::render_wrapper_chain(&chain, session, "%command%"))
    }
}

fn optional_number(value: Option<u32>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

/// The editable form of a profile's [`Diagnostics`] block.
///
/// Diagnostics are plain booleans, but they are edited and saved like the
/// wrapper chain: the draft converts back into the profile, and `apply_to`
/// validates on a copy so a refused save never half-edits the profile. The
/// environment itself is produced by `platform::envc`; this model only decides
/// which toggles the user has set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiagnosticsDraft {
    pub dxvk_hud: bool,
    pub mangohud: bool,
    pub proton_log: bool,
    pub dxvk_log: bool,
    pub fsr4_indicator: bool,
    pub dlss_indicator: bool,
}

/// One row in the diagnostics editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticToggle {
    DxvkHud,
    Mangohud,
    ProtonLog,
    DxvkLog,
    Fsr4Indicator,
    DlssIndicator,
}

impl DiagnosticToggle {
    /// Every toggle, in the order the editor shows them.
    pub const ALL: [Self; 6] = [
        Self::DxvkHud,
        Self::Mangohud,
        Self::ProtonLog,
        Self::DxvkLog,
        Self::Fsr4Indicator,
        Self::DlssIndicator,
    ];

    /// The short label on the toggle chip.
    pub const fn label(self) -> &'static str {
        match self {
            Self::DxvkHud => "DXVK HUD",
            Self::Mangohud => "MangoHUD",
            Self::ProtonLog => "Proton log",
            Self::DxvkLog => "DXVK log",
            Self::Fsr4Indicator => "FSR 4 indicator",
            Self::DlssIndicator => "DLSS indicator",
        }
    }

    /// The environment value this toggle emits, shown next to the chip.
    pub const fn env_hint(self) -> &'static str {
        match self {
            Self::DxvkHud => "DXVK_HUD=fps",
            Self::Mangohud => "MANGOHUD=1",
            Self::ProtonLog => "PROTON_LOG=1",
            Self::DxvkLog => "DXVK_LOG_LEVEL=debug",
            Self::Fsr4Indicator => "PROTON_FSR4_INDICATOR=1",
            Self::DlssIndicator => "PROTON_DLSS_INDICATOR=1",
        }
    }
}

impl DiagnosticsDraft {
    /// Load the editor from a stored profile.
    pub fn from_diagnostics(diagnostics: &Diagnostics) -> Self {
        Self {
            dxvk_hud: diagnostics.dxvk_hud,
            mangohud: diagnostics.mangohud,
            proton_log: diagnostics.proton_log,
            dxvk_log: diagnostics.dxvk_log,
            fsr4_indicator: diagnostics.fsr4_indicator,
            dlss_indicator: diagnostics.dlss_indicator,
        }
    }

    /// Convert the editor back into the profile's diagnostics block.
    pub fn to_diagnostics(self) -> Diagnostics {
        Diagnostics {
            dxvk_hud: self.dxvk_hud,
            mangohud: self.mangohud,
            proton_log: self.proton_log,
            dxvk_log: self.dxvk_log,
            fsr4_indicator: self.fsr4_indicator,
            dlss_indicator: self.dlss_indicator,
        }
    }

    /// The current value of one toggle.
    pub fn is_on(self, toggle: DiagnosticToggle) -> bool {
        match toggle {
            DiagnosticToggle::DxvkHud => self.dxvk_hud,
            DiagnosticToggle::Mangohud => self.mangohud,
            DiagnosticToggle::ProtonLog => self.proton_log,
            DiagnosticToggle::DxvkLog => self.dxvk_log,
            DiagnosticToggle::Fsr4Indicator => self.fsr4_indicator,
            DiagnosticToggle::DlssIndicator => self.dlss_indicator,
        }
    }

    /// Flip one toggle.
    pub fn toggle(&mut self, toggle: DiagnosticToggle) {
        self.set(toggle, !self.is_on(toggle));
    }

    /// Set one toggle.
    pub fn set(&mut self, toggle: DiagnosticToggle, value: bool) {
        match toggle {
            DiagnosticToggle::DxvkHud => self.dxvk_hud = value,
            DiagnosticToggle::Mangohud => self.mangohud = value,
            DiagnosticToggle::ProtonLog => self.proton_log = value,
            DiagnosticToggle::DxvkLog => self.dxvk_log = value,
            DiagnosticToggle::Fsr4Indicator => self.fsr4_indicator = value,
            DiagnosticToggle::DlssIndicator => self.dlss_indicator = value,
        }
    }

    /// `true` when no diagnostic is enabled.
    pub fn is_empty(self) -> bool {
        DiagnosticToggle::ALL
            .iter()
            .all(|toggle| !self.is_on(*toggle))
    }

    /// The labels of the enabled toggles, for a compact summary line.
    pub fn enabled_labels(self) -> Vec<&'static str> {
        DiagnosticToggle::ALL
            .iter()
            .filter(|toggle| self.is_on(**toggle))
            .map(|toggle| toggle.label())
            .collect()
    }

    /// Write the diagnostics into a profile and validate the result.
    ///
    /// Validation happens on a copy, so a refused draft never leaves the profile
    /// half-edited (diagnostics cannot fail validation today, but the profile
    /// carries other fields and the contract must not depend on that).
    pub fn apply_to(self, profile: &mut Profile) -> Result<(), String> {
        let mut next = profile.clone();
        next.diagnostics = self.to_diagnostics();
        next.validate().map_err(|error| error.to_string())?;
        *profile = next;
        Ok(())
    }
}

/// Render a compiled runtime as the command the shim will run.
///
/// This is the GUI's mirror of `pitagames-shim --explain`: the diagnostic
/// environment, `pitagames-shim <appid>`, the wrapper chain and `%command%`,
/// POSIX-quoted so it can be pasted back into a shell. It is display-only; the
/// actual launch stays argv-only.
#[must_use]
pub fn resolved_command(app_id: &str, compiled: &CompiledRuntime) -> String {
    let (program, args) = flatten_wrappers(&compiled.wrappers, "%command%", &[]);
    let mut shim_args = Vec::with_capacity(args.len() + 2);
    shim_args.push(app_id.to_string());
    shim_args.push(program);
    shim_args.extend(args);
    render_shell_command(&compiled.env, SHIM_PROGRAM, &shim_args)
}

/// Compile a profile against the current discovery and render its command.
pub fn resolved_preview(
    app_id: &str,
    profile: &Profile,
    report: &DiscoveryReport,
) -> Result<String, String> {
    let compiled = compile(profile, report)?;
    Ok(resolved_command(app_id, &compiled))
}

/// Render an argv list back into the editor's single text field.
fn render_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_args(text: &str, label: &str) -> Result<Vec<String>, String> {
    tokenize_command(text).map_err(|error| format!("{label}: {error}"))
}

/// Parse an optional gamescope dimension: blank means "not set".
fn parse_dimension(text: &str, label: &str) -> Result<Option<u32>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let value = text
        .parse::<u32>()
        .map_err(|_| format!("{label} must be a whole number."))?;
    if value == 0 {
        return Err(format!("{label} must be greater than zero."));
    }
    Ok(Some(value))
}

fn profiles_dir() -> Result<PathBuf, String> {
    pitagames_core::paths::profiles_dir()
        .ok_or_else(|| "no data directory (HOME is unset)".to_string())
}

fn logs_dir() -> Option<PathBuf> {
    pitagames_core::paths::logs_dir()
}

fn backup_dir() -> Result<PathBuf, String> {
    pitagames_core::paths::steam_backup_dir()
        .ok_or_else(|| "no data directory (HOME is unset)".to_string())
}

/// Compile the runtime subset of a profile against the current discovery.
///
/// This carries no GPU or compat context, so a profile that asks for a graphics
/// field is refused by name ([`envc::EnvcError::Unavailable`]). The Graphics
/// screen uses [`compile_with_graphics`] instead; the two share one compiler and
/// one renderer, so a preview and a launch cannot drift.
pub fn compile(profile: &Profile, report: &DiscoveryReport) -> Result<CompiledRuntime, String> {
    compile_with_graphics(
        profile,
        report,
        GraphicsContext::new(&GpuProfile::default(), None),
    )
}

/// Compile a profile with the detected GPU and the game's curated entry.
///
/// The Graphics screen's compile path: it hands the full [`GraphicsContext`] to
/// the one M3 compiler, so a graphics choice is checked against the capability
/// matrix exactly as a launch would check it.
pub fn compile_with_graphics(
    profile: &Profile,
    report: &DiscoveryReport,
    graphics: GraphicsContext<'_>,
) -> Result<CompiledRuntime, String> {
    let install = report
        .resolve(&profile.proton)
        .map_err(|error| error.to_string())?;
    let log_dir = logs_dir();
    envc::compile(
        CompileRequest {
            profile,
            proton: install.as_ref(),
            log_dir: log_dir.as_deref(),
            session: session_mode(),
        },
        graphics,
    )
    .map_err(|error| error.to_string())
}

/// Compile a profile and write its shim profile, returning both.
///
/// The written file is what `pitagames-shim` reads at launch; the compiled
/// runtime is returned so the UI can show the resolved Proton and environment.
pub fn write_shim_profile(
    profile: &Profile,
    report: &DiscoveryReport,
) -> Result<(PathBuf, CompiledRuntime), String> {
    write_shim_profile_in(&profiles_dir()?, profile, report)
}

/// Compile a profile with the graphics context and write its shim profile.
///
/// The Graphics screen's save path: a profile that asks for a graphics field
/// compiles against the detected GPU and the game's curated entry, so the file
/// the shim and the hand-off read is written rather than refused.
pub fn write_shim_profile_with_graphics(
    profile: &Profile,
    report: &DiscoveryReport,
    graphics: GraphicsContext<'_>,
) -> Result<(PathBuf, CompiledRuntime), String> {
    let dir = profiles_dir()?;
    let compiled = compile_with_graphics(profile, report, graphics)?;
    let shim = shim_from_compiled(profile, &compiled);
    let path = save_shim_profile(&dir, &shim).map_err(|error| error.to_string())?;
    Ok((path, compiled))
}

fn write_shim_profile_in(
    dir: &Path,
    profile: &Profile,
    report: &DiscoveryReport,
) -> Result<(PathBuf, CompiledRuntime), String> {
    let compiled = compile(profile, report)?;
    let shim = shim_from_compiled(profile, &compiled);
    let path = save_shim_profile(dir, &shim).map_err(|error| error.to_string())?;
    Ok((path, compiled))
}

/// Build the shim profile a launch or a hand-off reads from a compiled runtime.
///
/// The one place the compiled env, wrapper chain and resolved Proton become a
/// [`ShimProfile`], so the file a write produces and the preview a screen shows
/// are the same data.
fn shim_from_compiled(profile: &Profile, compiled: &CompiledRuntime) -> ShimProfile {
    let mut shim = ShimProfile::new(profile.game.clone(), &profile.name);
    shim.proton = Some(envc::shim_proton(&profile.proton, compiled.proton.as_ref()));
    shim.env = compiled.env.clone();
    shim.wrappers = compiled.wrappers.clone();
    shim.log_path = logs_dir().map(|dir| dir.join("shim.log"));
    shim
}

/// A non-Steam launch composed from a profile, for the Runtime panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffPreview {
    /// The launch shape: `umu`, `native` or `client`.
    pub mode: &'static str,
    /// The per-game Wine prefix, when the launcher owns one (a store client
    /// owns its own runtime and carries none).
    pub prefix: Option<String>,
    /// The POSIX-quoted command the hand-off will run.
    pub command: String,
}

/// A resolved-command preview: the one line the launcher would run.
///
/// The Graphics screen shows this for either shell; the Runtime panel shows the
/// non-Steam shape. Both come from [`compose_preview`], so a preview and a real
/// launch cannot fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsPreview {
    /// The launch shape for a non-Steam row (`umu`/`native`/`client`); `None`
    /// for a Steam row, whose line is the shim invocation.
    pub mode: Option<&'static str>,
    /// The per-game Wine prefix, when the launcher owns one.
    pub prefix: Option<String>,
    /// The POSIX-quoted command.
    pub command: String,
}

/// Render a compiled runtime as the command the launcher would run for `game`.
///
/// A Steam row renders the shim line ([`resolved_command`]); every other row
/// runs the same [`compose_handoff`] the Play button runs. One renderer, two
/// callers.
fn compose_preview(
    game: &Game,
    app_id: &str,
    profile: &Profile,
    compiled: CompiledRuntime,
    report: &DiscoveryReport,
) -> Result<GraphicsPreview, String> {
    if game.id.store == Store::Steam {
        return Ok(GraphicsPreview {
            mode: None,
            prefix: None,
            command: resolved_command(app_id, &compiled),
        });
    }

    let shim = shim_from_compiled(profile, &compiled);
    let prepared = compose_handoff(game, Some(&shim), report)?;
    Ok(GraphicsPreview {
        mode: Some(prepared.mode.label()),
        prefix: prepared
            .prefix
            .as_ref()
            .map(|path| path.display().to_string()),
        command: prepared.render(),
    })
}

/// The Graphics screen's resolved-command preview.
///
/// The full M3 compile against the detected GPU and the game's curated entry,
/// rendered by the same code the detail page's Command panel and a real launch
/// use. It writes nothing, so unsaved draft edits can be previewed.
pub fn graphics_preview(
    game: &Game,
    app_id: &str,
    profile: &Profile,
    report: &DiscoveryReport,
    graphics: GraphicsContext<'_>,
) -> Result<GraphicsPreview, String> {
    let compiled = compile_with_graphics(profile, report, graphics)?;
    compose_preview(game, app_id, profile, compiled, report)
}

/// Compose a non-Steam row's hand-off from a profile, for the preview.
///
/// The GUI's mirror of `pitagames-cli launch`: it compiles the profile with the
/// same [`envc`] compiler the shim path uses, then runs the same
/// [`compose_handoff`] the Play button runs — so the line the panel shows and
/// the process a launch starts cannot drift. It writes nothing, so unsaved
/// draft edits can be previewed.
pub fn handoff_preview(
    game: &Game,
    profile: &Profile,
    report: &DiscoveryReport,
) -> Result<HandoffPreview, String> {
    let compiled = compile(profile, report)?;
    let preview = compose_preview(game, game.id.store_id.as_str(), profile, compiled, report)?;
    Ok(HandoffPreview {
        mode: preview.mode.unwrap_or("native"),
        prefix: preview.prefix,
        command: preview.command,
    })
}

/// Compose the umu/Proton hand-off for a non-Steam row.
///
/// Shared by the Runtime panel's preview and [`crate::play`]'s worker, so the
/// line the panel shows and the process a Play starts cannot drift. `runtime` is
/// the compiled profile to apply — the saved shim file for a launch, the live
/// draft for a preview.
pub fn compose_handoff(
    game: &Game,
    runtime: Option<&ShimProfile>,
    report: &DiscoveryReport,
) -> Result<Prepared, String> {
    // A native executable runs without Proton, so discovery failing on a machine
    // that has none must not stop the launch: only a Windows build needs the
    // answer, and `prepare` names that failure itself.
    let proton = match resolve_handoff_proton(runtime, report) {
        Ok(install) => install,
        Err(error) if needs_proton(game) => return Err(error),
        Err(_) => None,
    };

    let root = pitagames_platform::prefix::default_root()
        .ok_or_else(|| "no data directory (HOME is unset)".to_string())?;
    // A store whose client starts games by id (gogdl, nile) names no
    // executable, so its argv is composed here and the hand-off only supervises
    // it.
    let client = client_launch(game)?;

    handoff::prepare(&handoff::Handoff {
        game,
        runtime,
        proton: proton.as_ref(),
        prefix_root: &root,
        session: session_mode(),
        client: client.as_ref(),
    })
    .map_err(|error| error.to_string())
}

/// The Proton a hand-off uses: the one the profile recorded, else discovery.
fn resolve_handoff_proton(
    runtime: Option<&ShimProfile>,
    report: &DiscoveryReport,
) -> Result<Option<ProtonInstall>, String> {
    let recorded = runtime
        .and_then(|runtime| runtime.proton.as_ref())
        .and_then(|proton| proton.install.as_ref());

    if let Some(recorded) = recorded
        && let Ok(install) = ProtonInstall::from_dir(&recorded.path, ProtonSource::Explicit)
    {
        return Ok(Some(install));
    }

    let reference = runtime
        .and_then(|runtime| runtime.proton.as_ref())
        .map(|proton| proton.reference.clone())
        .unwrap_or(ProtonRef::Auto);

    report
        .resolve(&reference)
        .map_err(|error| error.to_string())
}

/// Whether this row's launch needs a Proton build at all.
fn needs_proton(game: &Game) -> bool {
    game.exe
        .as_deref()
        .is_some_and(|exe| handoff::is_windows(exe, game))
}

/// The store client's argv, when this row launches by id instead of by path.
///
/// `None` for every other row, which keeps the umu/native hand-off untouched. A
/// template failure (GOG's `gogdl` needs the install directory) is returned as
/// the launch's own error, because it is exactly the case Play cannot retry
/// around.
pub fn client_launch(game: &Game) -> Result<Option<LaunchSpec>, String> {
    if game.exe.is_some() || !pitagames_stores::launch::launches_by_id(game.id.store) {
        return Ok(None);
    }
    let backends = Backends::new();
    pitagames_stores::launch::client_spec(&backends, game)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// The Steam user whose `localconfig.vdf` the launch options live in.
///
/// A machine normally has one; several users is the first by id, matching the
/// CLI's behaviour.
pub fn resolve_user(steam_root: &Path) -> Result<(String, PathBuf), String> {
    user_configs(steam_root)
        .into_iter()
        .next()
        .ok_or_else(|| "no Steam user with a localconfig.vdf; sign in to Steam once".to_string())
}

/// What the routing panel shows for one game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingStatus {
    /// `None` when no Steam user config exists yet.
    pub localconfig: Option<PathBuf>,
    pub user_id: Option<String>,
    /// The current `LaunchOptions` value for this app.
    pub launch_options: Option<String>,
    /// `true` when the value already routes the game through the shim.
    pub shim_installed: bool,
    pub steam: SteamProcessState,
    /// A read/parse problem; the rest of the status is still usable.
    pub warning: Option<String>,
}

/// Read the current routing state without changing anything.
pub fn routing_status(steam_root: &Path, app_id: &str) -> RoutingStatus {
    let steam = steam_config::detect_steam_process(steam_root);

    let (user_id, localconfig) = match resolve_user(steam_root) {
        Ok((user, path)) => (Some(user), Some(path)),
        Err(_) => (None, None),
    };

    let mut status = RoutingStatus {
        localconfig,
        user_id,
        launch_options: None,
        shim_installed: false,
        steam,
        warning: None,
    };

    let Some(localconfig) = status.localconfig.clone() else {
        status.warning = Some("No Steam user config found.".to_string());
        return status;
    };

    match steam_config::read_launch_options(&localconfig, app_id) {
        Ok(value) => {
            status.shim_installed = value
                .as_deref()
                .is_some_and(|value| routes_through_shim(app_id, value));
            status.launch_options = value;
        }
        Err(error) => status.warning = Some(error.to_string()),
    }

    status
}

fn routes_through_shim(app_id: &str, value: &str) -> bool {
    // `shim_launch_options` is idempotent: composing it over a value that
    // already routes through this appid's shim returns the same string.
    shim_launch_options(app_id, value) == value.trim()
}

/// Route the game through the shim in Steam's launch options.
pub fn apply(steam_root: &Path, app_id: &str, force: bool) -> Result<WriteReport, String> {
    apply_in(steam_root, app_id, force, &backup_dir()?)
}

fn apply_in(
    steam_root: &Path,
    app_id: &str,
    force: bool,
    backup_dir: &Path,
) -> Result<WriteReport, String> {
    let (_user, localconfig) = resolve_user(steam_root)?;
    let steam = steam_config::detect_steam_process(steam_root);
    let current = steam_config::read_launch_options(&localconfig, app_id)
        .map_err(|error| error.to_string())?;
    let next = shim_launch_options(app_id, current.as_deref().unwrap_or_default());

    let mut options = WriteOptions::new(backup_dir);
    options.allow_running = force;

    steam_config::apply_launch_options(&localconfig, app_id, Some(&next), steam, &options)
        .map_err(|error| error.to_string())
}

/// Put the game's original launch options back.
pub fn restore(steam_root: &Path, app_id: &str, force: bool) -> Result<RestoreReport, String> {
    restore_in(steam_root, app_id, force, &backup_dir()?)
}

fn restore_in(
    steam_root: &Path,
    app_id: &str,
    force: bool,
    backup_dir: &Path,
) -> Result<RestoreReport, String> {
    let (_user, localconfig) = resolve_user(steam_root)?;
    let steam = steam_config::detect_steam_process(steam_root);

    let mut options = WriteOptions::new(backup_dir);
    options.allow_running = force;

    steam_config::restore_launch_options(&localconfig, app_id, steam, &options)
        .map_err(|error| error.to_string())
}

/// `<steam>/config/config.vdf`, where the per-game compatibility tool lives.
pub fn compat_config_path(steam_root: &Path) -> PathBuf {
    steam_root.join("config").join("config.vdf")
}

/// The compatibility tool Steam is forced to use for this game, if any.
pub fn read_compat(steam_root: &Path, app_id: &str) -> Result<Option<CompatTool>, String> {
    let config = compat_config_path(steam_root);
    if !config.is_file() {
        return Ok(None);
    }
    steam_config::read_compat_tool(&config, app_id).map_err(|error| error.to_string())
}

/// Force a compatibility tool for this game (`config.vdf` `CompatToolMapping`).
pub fn apply_compat(
    steam_root: &Path,
    app_id: &str,
    tool: &CompatTool,
    force: bool,
) -> Result<CompatWriteReport, String> {
    compat_write(steam_root, app_id, Some(tool), force, &backup_dir()?)
}

/// Remove the forced tool, so Steam decides again.
pub fn clear_compat(
    steam_root: &Path,
    app_id: &str,
    force: bool,
) -> Result<CompatWriteReport, String> {
    compat_write(steam_root, app_id, None, force, &backup_dir()?)
}

/// Put the game's original compatibility-tool mapping back.
pub fn restore_compat(
    steam_root: &Path,
    app_id: &str,
    force: bool,
) -> Result<CompatRestoreReport, String> {
    restore_compat_in(steam_root, app_id, force, &backup_dir()?)
}

fn restore_compat_in(
    steam_root: &Path,
    app_id: &str,
    force: bool,
    backup_dir: &Path,
) -> Result<CompatRestoreReport, String> {
    let config = compat_config_path(steam_root);
    let steam = steam_config::detect_steam_process(steam_root);
    let mut options = WriteOptions::new(backup_dir);
    options.allow_running = force;

    steam_config::restore_compat_tool(&config, app_id, steam, &options)
        .map_err(|error| error.to_string())
}

fn compat_write(
    steam_root: &Path,
    app_id: &str,
    tool: Option<&CompatTool>,
    force: bool,
    backup_dir: &Path,
) -> Result<CompatWriteReport, String> {
    let config = compat_config_path(steam_root);
    let steam = steam_config::detect_steam_process(steam_root);
    let mut options = WriteOptions::new(backup_dir);
    options.allow_running = force;

    steam_config::apply_compat_tool(&config, app_id, tool, steam, &options)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{GameId, Store};
    use pitagames_platform::proton::{ProtonInstall, ProtonKind, ProtonSource};

    fn game() -> GameId {
        GameId::new(Store::Steam, "620")
    }

    fn cachyos() -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
            name: "proton-cachyos".to_string(),
            kind: ProtonKind::CachyOs,
            version: Some("proton-cachyos 10.0-20251101".to_string()),
            source: ProtonSource::SystemCompatibilityTools,
        }
    }

    #[test]
    fn the_wrapper_draft_round_trips_a_stored_chain() {
        let chain = WrapperChain {
            gamemode: true,
            mangohud: true,
            gamescope: Some(GamescopeOptions {
                width: Some(2560),
                height: Some(1440),
                refresh_hz: Some(165),
                fullscreen: false,
                extra_args: vec!["--hdr-enabled".into(), "two words".into()],
            }),
            custom: vec![WrapperStep {
                program: PathBuf::from("/usr/bin/mangohud"),
                args: vec!["--dls".into(), "it's".into()],
            }],
        };

        let draft = WrapperDraft::from_chain(&chain);
        assert_eq!(draft.width, "2560");
        assert_eq!(draft.extra_args, "--hdr-enabled 'two words'");
        assert_eq!(draft.custom[0].args, "--dls 'it'\\''s'");
        assert_eq!(draft.to_chain().unwrap(), chain);
    }

    #[test]
    fn the_wrapper_draft_reports_bad_input_instead_of_saving_it() {
        let mut draft = WrapperDraft::default();
        assert_eq!(draft.to_chain().unwrap(), WrapperChain::default());

        draft.gamescope = true;
        draft.width = "wide".into();
        assert_eq!(
            draft.to_chain().unwrap_err(),
            "Gamescope width must be a whole number."
        );

        draft.width = "0".into();
        assert_eq!(
            draft.to_chain().unwrap_err(),
            "Gamescope width must be greater than zero."
        );

        draft.width.clear();
        draft.extra_args = "\"unterminated".into();
        assert!(
            draft
                .to_chain()
                .unwrap_err()
                .starts_with("Gamescope extra args: unterminated quote")
        );

        draft.extra_args.clear();
        draft.custom.push(WrapperDraftStep {
            program: "  ".into(),
            args: String::new(),
        });
        assert_eq!(
            draft.to_chain().unwrap_err(),
            "Custom wrapper #1 needs a program."
        );

        let mut draft = WrapperDraft::default();
        assert_eq!(
            draft.add_step("  ", "").unwrap_err(),
            "A custom wrapper needs a program."
        );
        assert!(
            draft
                .add_step("gamemoderun", "\"oops")
                .unwrap_err()
                .starts_with("Custom wrapper args: unterminated quote")
        );
        draft.add_step("gamemoderun", "--dls 'two words'").unwrap();
        assert_eq!(draft.custom[0].args, "--dls 'two words'");
    }

    #[test]
    fn the_wrapper_preview_shows_the_flattened_chain() {
        let mut draft = WrapperDraft {
            gamemode: true,
            mangohud: true,
            gamescope: true,
            width: "1920".into(),
            height: "1080".into(),
            refresh_hz: "120".into(),
            ..WrapperDraft::default()
        };
        draft.add_step("prism-run", "--prefer-wayland").unwrap();

        assert_eq!(
            draft.preview_in(false).unwrap(),
            "gamescope -w 1920 -h 1080 -r 120 --fullscreen -- gamemoderun mangohud \
             prism-run --prefer-wayland %command%"
        );
        // The console session owns the compositor, so the per-game step is
        // suppressed in the preview exactly as it is at launch.
        assert_eq!(
            draft.preview_in(true).unwrap(),
            "gamemoderun mangohud prism-run --prefer-wayland %command%"
        );
    }

    #[test]
    fn applying_the_draft_updates_the_profile_and_validates_it() {
        let mut profile = Profile::new(game(), "Wrapped");
        let draft = WrapperDraft {
            gamemode: true,
            mangohud: true,
            ..WrapperDraft::default()
        };
        draft.apply_to(&mut profile).unwrap();
        assert!(profile.wrappers.gamemode);
        assert!(profile.wrappers.mangohud);
        assert!(profile.wrappers.gamescope.is_none());

        let draft = WrapperDraft {
            gamescope: true,
            width: "0".into(),
            ..WrapperDraft::default()
        };
        assert!(draft.apply_to(&mut profile).is_err());
        // A refused draft must not mutate the profile.
        assert!(profile.wrappers.gamescope.is_none());
    }

    #[test]
    fn compiling_a_profile_writes_the_shim_file_and_env() {
        let temp = tempfile::tempdir().unwrap();
        let mut profile = Profile::new(game(), "Default");
        profile.diagnostics.dxvk_hud = true;
        profile.diagnostics.proton_log = true;

        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };

        let (path, compiled) = write_shim_profile_in(temp.path(), &profile, &report).unwrap();
        assert_eq!(path, temp.path().join("steam-620.json"));
        assert_eq!(compiled.env[0], ("DXVK_HUD".to_string(), "fps".to_string()));
        assert!(
            compiled.proton.is_some(),
            "auto resolved the discovered build"
        );

        let stored = pitagames_core::load_shim_profile(temp.path(), &game())
            .unwrap()
            .unwrap();
        assert_eq!(stored.profile, "Default");
        assert_eq!(
            stored
                .proton
                .as_ref()
                .unwrap()
                .install
                .as_ref()
                .unwrap()
                .name,
            "proton-cachyos"
        );
        assert!(stored.env.iter().any(|(key, _)| key == "DXVK_HUD"));
    }

    #[test]
    fn a_graphics_profile_is_reported_rather_than_written() {
        let temp = tempfile::tempdir().unwrap();
        let mut profile = Profile::new(game(), "FSR 4");
        profile.upscaler = pitagames_core::Upscaler::Fsr4Upgrade { version: None };

        // A resolvable Proton is present, so the refusal is about graphics and
        // not about a missing runtime.
        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };

        let error = write_shim_profile_in(temp.path(), &profile, &report).unwrap_err();
        // The M3 compiler refuses the field by name with the capability
        // matrix's own reason (the runtime-subset compile carries no GPU, so the
        // card reads as unrecognised).
        assert!(error.contains("FSR 4 upgrade"), "{error}");
        assert!(error.contains("not available"), "{error}");
        assert!(
            !temp.path().join("steam-620.json").exists(),
            "a refused profile must not leave a file behind"
        );
    }

    #[test]
    fn apply_and_restore_round_trip_through_the_writer() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Steam");
        let config = root.join("userdata/123/config");
        std::fs::create_dir_all(&config).unwrap();
        let localconfig = config.join("localconfig.vdf");
        let original = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"620\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"Playtime\"\t\t\"5\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        std::fs::write(&localconfig, original).unwrap();

        let backups = temp.path().join("backups");

        let status = routing_status(&root, "620");
        assert_eq!(status.user_id.as_deref(), Some("123"));
        assert!(!status.shim_installed);
        assert_eq!(status.launch_options, None);

        let written = apply_in(&root, "620", false, &backups).unwrap();
        assert!(written.changed);
        assert_eq!(
            written.applied.as_deref(),
            Some("pitagames-shim 620 %command%")
        );
        assert!(
            routing_status(&root, "620").shim_installed,
            "the status must see the shim after applying"
        );

        let restored = restore_in(&root, "620", false, &backups).unwrap();
        assert!(restored.exact, "nothing else changed, so a byte restore");
        assert_eq!(std::fs::read_to_string(&localconfig).unwrap(), original);
        assert!(!routing_status(&root, "620").shim_installed);
    }

    #[test]
    fn forcing_a_compat_tool_round_trips_through_the_writer() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Steam");
        let config = root.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let config_vdf = config.join("config.vdf");
        let original = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"CompatToolMapping\"\n\t\t\t\t{\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        std::fs::write(&config_vdf, original).unwrap();

        let backups = temp.path().join("backups");

        assert_eq!(read_compat(&root, "620").unwrap(), None);

        let written = compat_write(
            &root,
            "620",
            Some(&CompatTool::new("proton-cachyos")),
            true,
            &backups,
        )
        .unwrap();
        assert!(written.changed);
        assert_eq!(
            read_compat(&root, "620").unwrap().unwrap().name,
            "proton-cachyos"
        );

        let cleared = compat_write(&root, "620", None, true, &backups).unwrap();
        assert!(cleared.changed);
        assert_eq!(read_compat(&root, "620").unwrap(), None);

        // The restore record points at the pre-PitaGames (empty) state.
        restore_compat_in(&root, "620", true, &backups).unwrap();
        assert_eq!(std::fs::read_to_string(&config_vdf).unwrap(), original);
    }

    #[test]
    fn the_diagnostics_draft_round_trips_and_toggles() {
        let diagnostics = Diagnostics {
            dxvk_hud: true,
            mangohud: true,
            proton_log: true,
            dxvk_log: true,
            fsr4_indicator: true,
            dlss_indicator: true,
        };
        let mut draft = DiagnosticsDraft::from_diagnostics(&diagnostics);
        assert_eq!(draft.to_diagnostics(), diagnostics);
        assert_eq!(draft.enabled_labels().len(), 6);

        // Every toggle maps to a distinct field.
        for toggle in DiagnosticToggle::ALL {
            assert!(draft.is_on(toggle), "{toggle:?}");
            draft.toggle(toggle);
            assert!(!draft.is_on(toggle), "{toggle:?}");
        }
        assert!(draft.is_empty());
        assert!(draft.enabled_labels().is_empty());
        assert_eq!(draft.to_diagnostics(), Diagnostics::default());
    }

    #[test]
    fn applying_the_diagnostics_draft_updates_the_profile() {
        let mut profile = Profile::new(game(), "Debug");
        let mut draft = DiagnosticsDraft::default();
        draft.set(DiagnosticToggle::DxvkHud, true);
        draft.set(DiagnosticToggle::ProtonLog, true);
        draft.apply_to(&mut profile).unwrap();

        assert!(profile.diagnostics.dxvk_hud);
        assert!(profile.diagnostics.proton_log);
        assert!(!profile.diagnostics.dlss_indicator);
    }

    #[test]
    fn the_resolved_command_matches_the_shim_composition() {
        let mut profile = Profile::new(game(), "Debug");
        profile.diagnostics.dxvk_hud = true;
        profile.wrappers.gamemode = true;
        profile.wrappers.gamescope = Some(GamescopeOptions {
            width: Some(1920),
            height: Some(1080),
            refresh_hz: None,
            fullscreen: true,
            extra_args: Vec::new(),
        });

        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };
        let command = resolved_preview("620", &profile, &report).unwrap();
        assert_eq!(
            command,
            "DXVK_HUD=fps pitagames-shim 620 gamescope -w 1920 -h 1080 --fullscreen -- \
             gamemoderun %command%"
        );
    }

    #[test]
    fn a_profile_with_no_diagnostics_resolves_to_the_bare_shim() {
        let profile = Profile::new(game(), "Default");
        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };
        assert_eq!(
            resolved_preview("620", &profile, &report).unwrap(),
            "pitagames-shim 620 %command%"
        );
    }

    #[test]
    fn a_graphics_profile_cannot_be_previewed() {
        let mut profile = Profile::new(game(), "FSR 4");
        profile.upscaler = pitagames_core::Upscaler::Fsr4Upgrade { version: None };
        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };
        let error = resolved_preview("620", &profile, &report).unwrap_err();
        assert!(error.contains("FSR 4 upgrade"), "{error}");
        assert!(error.contains("not available"), "{error}");
    }

    // -- The Graphics screen's one composition -----------------------------

    fn rdna4() -> GpuProfile {
        GpuProfile {
            vendor: pitagames_platform::gpu::GpuArch::Rdna4.vendor(),
            arch: pitagames_platform::gpu::GpuArch::Rdna4,
            device_id: None,
            name: None,
            mesa: Some(pitagames_platform::compat::Version::parse("27.0.0").unwrap()),
            vram_bytes: None,
            ntsync: false,
            hdr: pitagames_platform::gpu::HdrSupport::Unknown,
            warnings: Vec::new(),
        }
    }

    fn cyberpunk() -> pitagames_platform::compat::CompatEntry {
        pitagames_platform::compat::CompatEntry::from_toml(
            "[game]\nstore = \"steam\"\nstore_id = \"620\"\ntitle = \"Cyberpunk 2077\"\n\n\
             [upscaling]\nsupported = [\"dlss\"]\n",
        )
        .unwrap()
    }

    fn steam_game() -> Game {
        let mut game = Game::new(game(), "Cyberpunk 2077");
        game.installed = true;
        game
    }

    #[test]
    fn a_graphics_profile_previews_with_the_gpu_context() {
        // The same profile the runtime-subset compile refuses compiles here,
        // because the graphics context says the card offers FSR 4. The line is
        // the shim invocation, rendered by the same `resolved_command`.
        let mut profile = Profile::new(game(), "FSR 4");
        profile.upscaler = pitagames_core::Upscaler::Fsr4Upgrade { version: None };
        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };
        let gpu = rdna4();
        let compat = cyberpunk();

        let preview = graphics_preview(
            &steam_game(),
            "620",
            &profile,
            &report,
            GraphicsContext::new(&gpu, Some(&compat)),
        )
        .unwrap();
        assert_eq!(preview.mode, None, "a Steam row renders the shim line");
        assert_eq!(preview.prefix, None);
        assert!(
            preview.command.contains("PROTON_FSR4_UPGRADE=1"),
            "{}",
            preview.command
        );
        assert!(
            preview.command.contains("pitagames-shim 620"),
            "{}",
            preview.command
        );
    }

    #[test]
    fn a_hidden_field_is_refused_by_name_with_the_matrix_reason() {
        let mut profile = Profile::new(game(), "FSR 4");
        profile.upscaler = pitagames_core::Upscaler::Fsr4Upgrade { version: None };
        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };
        // An unrecognised card hides FSR 4; the preview says so instead of
        // rendering a line that would not inject.
        let error = graphics_preview(
            &steam_game(),
            "620",
            &profile,
            &report,
            GraphicsContext::new(&GpuProfile::default(), Some(&cyberpunk())),
        )
        .unwrap_err();
        assert!(
            error.contains(pitagames_platform::capability::reason::GPU_UNKNOWN),
            "{error}"
        );
    }

    #[test]
    fn a_non_steam_graphics_row_previews_the_hand_off() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Gizmo");
        std::fs::create_dir_all(&install).unwrap();
        let exe = install.join("Gizmo.exe");
        std::fs::write(&exe, "").unwrap();
        let mut game = epic_game("Gizmo", Some(exe.to_str().unwrap()));
        game.install_dir = Some(install);

        let mut profile = Profile::new(game.id.clone(), "FSR 4");
        profile.upscaler = pitagames_core::Upscaler::Fsr4Upgrade { version: None };

        let preview = graphics_preview(
            &game,
            &game.id.store_id,
            &profile,
            &report(),
            GraphicsContext::new(&rdna4(), None),
        )
        .unwrap();
        assert_eq!(preview.mode, Some("umu"));
        assert!(
            preview.command.contains("PROTON_FSR4_UPGRADE=1"),
            "{}",
            preview.command
        );
        assert!(preview.command.contains("umu-run"), "{}", preview.command);
    }

    #[test]
    fn the_graphics_write_accepts_a_profile_the_runtime_subset_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let mut profile = Profile::new(game(), "FSR 4");
        profile.upscaler = pitagames_core::Upscaler::Fsr4Upgrade { version: None };
        let report = DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        };

        // The runtime-subset write refuses it…
        assert!(write_shim_profile_in(temp.path(), &profile, &report).is_err());

        // …and the graphics write does not, writing the shim file.
        let dir = temp.path();
        let compiled = compile_with_graphics(
            &profile,
            &report,
            GraphicsContext::new(&rdna4(), Some(&cyberpunk())),
        )
        .unwrap();
        assert!(
            compiled
                .env
                .iter()
                .any(|(key, _)| key == "PROTON_FSR4_UPGRADE")
        );
        let shim = shim_from_compiled(&profile, &compiled);
        let path = save_shim_profile(dir, &shim).unwrap();
        assert!(path.exists());
        let stored = pitagames_core::load_shim_profile(dir, &game())
            .unwrap()
            .unwrap();
        assert!(
            stored
                .env
                .iter()
                .any(|(key, _)| key == "PROTON_FSR4_UPGRADE")
        );
    }

    /// An installed non-Steam row with an optional executable.
    fn epic_game(id: &str, exe: Option<&str>) -> Game {
        let mut game = Game::new(GameId::new(Store::Epic, id), id);
        game.installed = true;
        game.exe = exe.map(PathBuf::from);
        game
    }

    /// A report with one discovered build; enough for a Windows hand-off.
    fn report() -> DiscoveryReport {
        DiscoveryReport {
            installs: vec![cachyos()],
            ..DiscoveryReport::default()
        }
    }

    #[test]
    fn only_a_windows_build_needs_proton() {
        let native = epic_game("Native", Some("/games/Native/native.sh"));
        assert!(!needs_proton(&native));

        let windows = epic_game("Windows", Some("/games/Windows/Game.exe"));
        assert!(needs_proton(&windows));

        let mut tagged = epic_game("Tagged", Some("/games/Tagged/Game"));
        tagged
            .tags
            .push(pitagames_platform::launch::WINDOWS_TAG.to_string());
        assert!(needs_proton(&tagged));

        assert!(
            !needs_proton(&epic_game("NoExe", None)),
            "prepare names that failure"
        );
    }

    #[test]
    fn a_store_that_launches_by_id_gets_a_client_spec() {
        let mut gog = Game::new(GameId::new(Store::Gog, "1207658930"), "GOG Game");
        gog.installed = true;
        gog.install_dir = Some(PathBuf::from("/games/gog/GOG Game"));

        let spec = client_launch(&gog).unwrap().expect("gogdl composes a spec");
        assert_eq!(spec.program, PathBuf::from("gogdl"));
        assert!(spec.args.contains(&"launch".to_string()), "{:?}", spec.args);
        assert_eq!(spec.cwd.as_deref(), Some(Path::new("/games/gog/GOG Game")));
    }

    #[test]
    fn client_launch_is_only_for_a_store_without_an_executable() {
        let epic = epic_game("Gizmo", Some("/games/Gizmo/Gizmo.exe"));
        assert!(client_launch(&epic).unwrap().is_none());

        assert!(
            client_launch(&epic_game("Gizmo", None)).unwrap().is_none(),
            "Epic launches through umu, not its client"
        );

        let mut gog = Game::new(GameId::new(Store::Gog, "1234"), "GOG Game");
        gog.installed = true;
        gog.exe = Some(PathBuf::from("/games/gog/game.exe"));
        assert!(
            client_launch(&gog).unwrap().is_none(),
            "a row with an executable goes through the hand-off directly"
        );
    }

    #[test]
    fn a_gog_row_without_a_directory_reports_the_template_error() {
        let mut gog = Game::new(GameId::new(Store::Gog, "1234"), "GOG Game");
        gog.installed = true;

        let error = client_launch(&gog).unwrap_err();
        assert!(error.contains("dir"), "{error}");
    }

    #[test]
    fn a_windows_row_previews_the_umu_hand_off() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Gizmo");
        std::fs::create_dir_all(&install).unwrap();
        let exe = install.join("Gizmo.exe");
        std::fs::write(&exe, "").unwrap();
        let mut game = epic_game("Gizmo", Some(exe.to_str().unwrap()));
        game.install_dir = Some(install);

        let mut profile = Profile::new(game.id.clone(), "Default");
        profile.diagnostics.dxvk_hud = true;

        let preview = handoff_preview(&game, &profile, &report()).unwrap();

        assert_eq!(preview.mode, "umu");
        assert!(
            preview.prefix.as_deref().unwrap().ends_with("epic-Gizmo"),
            "{:?}",
            preview.prefix
        );
        assert!(
            preview.command.contains("DXVK_HUD=fps"),
            "{}",
            preview.command
        );
        assert!(preview.command.contains("umu-run"), "{}", preview.command);
        assert!(
            preview.command.contains("GAMEID=epic-Gizmo"),
            "{}",
            preview.command
        );
        assert!(
            preview.command.contains("STORE=epic"),
            "{}",
            preview.command
        );
    }

    #[test]
    fn a_native_row_previews_a_direct_launch_without_proton() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("native");
        std::fs::write(&exe, "").unwrap();
        let mut game = Game::new(GameId::new(Store::Local, "native"), "Native");
        game.installed = true;
        game.exe = Some(exe.clone());

        let profile = Profile::new(game.id.clone(), "Default");
        // A machine with no Proton must still preview a native launch.
        let preview = handoff_preview(&game, &profile, &DiscoveryReport::default()).unwrap();

        assert_eq!(preview.mode, "native");
        assert_eq!(preview.prefix, None);
        assert!(
            preview.command.contains(&exe.display().to_string()),
            "{}",
            preview.command
        );
        assert!(!preview.command.contains("umu-run"), "{}", preview.command);
    }

    #[test]
    fn a_store_client_row_previews_its_client_launch() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("GOG Game");
        std::fs::create_dir_all(&install).unwrap();
        let mut game = Game::new(GameId::new(Store::Gog, "1207658930"), "GOG Game");
        game.installed = true;
        game.install_dir = Some(install);

        let profile = Profile::new(game.id.clone(), "Default");
        let preview = handoff_preview(&game, &profile, &DiscoveryReport::default()).unwrap();

        assert_eq!(preview.mode, "client");
        assert_eq!(preview.prefix, None, "a store client owns its own runtime");
        assert!(preview.command.contains("gogdl"), "{}", preview.command);
        assert!(
            preview.command.contains("1207658930"),
            "{}",
            preview.command
        );
    }

    #[test]
    fn a_not_installed_row_reports_the_hand_off_refusal() {
        let mut game = Game::new(GameId::new(Store::Epic, "Gizmo"), "Gizmo");
        game.installed = false;
        let profile = Profile::new(game.id.clone(), "Default");

        let error = handoff_preview(&game, &profile, &report()).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
    }

    #[test]
    fn a_graphics_profile_cannot_be_previewed_by_the_hand_off() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Gizmo");
        std::fs::create_dir_all(&install).unwrap();
        let exe = install.join("Gizmo.exe");
        std::fs::write(&exe, "").unwrap();
        let mut game = epic_game("Gizmo", Some(exe.to_str().unwrap()));
        game.install_dir = Some(install);

        let mut profile = Profile::new(game.id.clone(), "FSR 4");
        profile.upscaler = pitagames_core::Upscaler::Fsr4Upgrade { version: None };

        let error = handoff_preview(&game, &profile, &report()).unwrap_err();
        assert!(error.contains("FSR 4 upgrade"), "{error}");
        assert!(error.contains("not available"), "{error}");
    }
}
