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

use pitagames_core::{Profile, ShimProfile, save_shim_profile, shim_launch_options};
use pitagames_platform::envc::{self, CompileRequest, CompiledRuntime};
use pitagames_platform::proton::{self, DiscoveryReport};
use pitagames_stores::steam::user_configs;
use pitagames_stores::steam_config::{
    self, RestoreReport, SteamProcessState, WriteOptions, WriteReport,
};

/// Every Proton build discovery found, best first.
pub fn discover_proton() -> DiscoveryReport {
    proton::discover()
}

/// `true` under the console session, where the per-game gamescope wrapper is
/// suppressed (nested gamescope is a bug).
pub fn session_mode() -> bool {
    std::env::var_os("PITAGAMES_SESSION").is_some_and(|value| !value.is_empty())
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
pub fn compile(profile: &Profile, report: &DiscoveryReport) -> Result<CompiledRuntime, String> {
    let install = report
        .resolve(&profile.proton)
        .map_err(|error| error.to_string())?;
    let log_dir = logs_dir();
    envc::compile_runtime(CompileRequest {
        profile,
        proton: install.as_ref(),
        log_dir: log_dir.as_deref(),
        session: session_mode(),
    })
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

fn write_shim_profile_in(
    dir: &Path,
    profile: &Profile,
    report: &DiscoveryReport,
) -> Result<(PathBuf, CompiledRuntime), String> {
    let compiled = compile(profile, report)?;

    let mut shim = ShimProfile::new(profile.game.clone(), &profile.name);
    shim.proton = Some(envc::shim_proton(&profile.proton, compiled.proton.as_ref()));
    shim.env = compiled.env.clone();
    shim.wrappers = compiled.wrappers.clone();
    shim.log_path = logs_dir().map(|dir| dir.join("shim.log"));

    let path = save_shim_profile(dir, &shim).map_err(|error| error.to_string())?;
    Ok((path, compiled))
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
        assert!(error.contains("upscaler"), "{error}");
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
}
