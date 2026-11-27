//! The local settings export: a support blob with no endpoint and no secrets.
//!
//! A user filing a bug wants to show the maintainers what their machine looks
//! like — which GPU, which Proton, which environment a game resolves — without
//! handing over an account or an API key. This module renders exactly that as
//! plain text: the detected GPU, the discovered Proton builds, and the resolved
//! launch environment.
//!
//! Two rules are enforced here, not documented and hoped for:
//!
//! - **Local only.** There is no network call, no URL, no upload. The text is
//!   returned to the caller, which copies it to the clipboard or writes it to a
//!   file the user picks.
//! - **No credentials.** The environment kept is only the one the shim applies;
//!   any key that looks like a credential (a name containing `KEY`, `TOKEN`,
//!   `SECRET`, `PASSWORD`, `CREDENTIAL` or `AUTH`) is dropped rather than
//!   printed, so a stray variable can never leak.

use std::path::Path;

use pitagames_core::{GameId, ShimProfile, paths, read_shim_profile, shim_profile_path};
use pitagames_platform::disk::human_bytes;
use pitagames_platform::gpu::{self, GpuProbe, GpuProfile};
use pitagames_platform::proton::{self, ProtonInstall};

/// The substring tests that mark an environment variable as a credential.
///
/// Deliberately broad: dropping a harmless `MONKEY` variable is cheaper than
/// printing a real key, and the export is a diagnostic, not a faithful dump.
const SECRET_MARKERS: [&str; 6] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"];

/// Everything the export shows, borrowed so the caller keeps ownership.
pub struct ExportInput<'a> {
    /// The detected GPU.
    pub gpu: &'a GpuProfile,
    /// The Proton build PitaGames would use, if any.
    pub selected: Option<&'a ProtonInstall>,
    /// Every other Proton build discovery found, for context.
    pub protons: &'a [ProtonInstall],
    /// The game whose environment is shown, if the export is per-game.
    pub game: Option<&'a GameId>,
    /// The game's stored shim profile, if any.
    pub profile: Option<&'a ShimProfile>,
}

/// A rendered settings export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsExport {
    text: String,
}

impl SettingsExport {
    /// Render the export from a GPU, a Proton selection and a game's profile.
    #[must_use]
    pub fn render(input: &ExportInput<'_>) -> Self {
        let mut out = String::new();
        out.push_str("PitaGames settings export\n");
        out.push_str("Local only: no account data, no telemetry, nothing sent anywhere.\n");

        out.push_str("\n# GPU\n");
        let gpu = input.gpu;
        out.push_str(&format!("arch          {}\n", gpu.arch.as_str()));
        out.push_str(&format!(
            "name          {}\n",
            gpu.name.as_deref().unwrap_or("(unknown — no vulkaninfo)")
        ));
        out.push_str(&format!(
            "device id     {}\n",
            gpu.device_id
                .map(|id| format!("0x{id:04x}"))
                .unwrap_or_else(|| "(unknown)".to_string())
        ));
        out.push_str(&format!(
            "mesa          {}\n",
            gpu.mesa
                .map(|version| version.to_string())
                .unwrap_or_else(|| "(unknown)".to_string())
        ));
        out.push_str(&format!(
            "vram          {}\n",
            gpu.vram_bytes
                .map(human_bytes)
                .unwrap_or_else(|| "(unknown)".to_string())
        ));
        out.push_str(&format!(
            "ntsync        {}\n",
            if gpu.ntsync { "yes" } else { "no" }
        ));
        for warning in &gpu.warnings {
            out.push_str(&format!("warning       {warning}\n"));
        }

        out.push_str("\n# Proton\n");
        match input.selected {
            Some(install) => out.push_str(&proton_line("selected", install)),
            None => out.push_str("selected      (none discovered)\n"),
        }
        let mut others: Vec<&ProtonInstall> = input
            .protons
            .iter()
            .filter(|install| {
                input
                    .selected
                    .is_none_or(|selected| selected.path != install.path)
            })
            .collect();
        others.sort_by(|a, b| a.name.cmp(&b.name));
        for install in others {
            out.push_str(&proton_line("available", install));
        }

        out.push_str("\n# Resolved environment\n");
        match input.profile {
            Some(profile) => {
                let game = input
                    .game
                    .map(|id| format!("{} {}", id.store.as_str(), id.store_id))
                    .unwrap_or_else(|| "(unknown game)".to_string());
                out.push_str(&format!("game          {game}\n"));
                out.push_str(&format!("profile       {}\n", profile.profile));
                let mut entries: Vec<&(String, String)> = profile
                    .env
                    .iter()
                    .filter(|entry| !looks_secret(&entry.0))
                    .collect();
                entries.sort();
                if entries.is_empty() {
                    out.push_str("(no environment variables set)\n");
                } else {
                    for (name, value) in entries {
                        out.push_str(&format!("{name}={value}\n"));
                    }
                }
            }
            None => out.push_str("(no profile — nothing to resolve)\n"),
        }

        Self { text: out }
    }

    /// The rendered text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Build this machine's export: the detected GPU, the discovered Proton builds,
/// and — when a game is given — its stored shim profile.
///
/// `profiles_dir` overrides where the shim profile is read from (`None` uses the
/// XDG default), so the CLI's `--dir` and a screen's default agree. The one
/// entry point a Settings screen calls; it probes the GPU and the disk, so a
/// caller runs it off the UI thread.
#[must_use]
pub fn for_machine(selected: Option<&GameId>, profiles_dir: Option<&Path>) -> SettingsExport {
    let gpu = gpu::detect(&GpuProbe::host());
    let discovery = proton::discover();
    let selected_proton = discovery.best().cloned();
    let dir = profiles_dir
        .map(Path::to_path_buf)
        .or_else(paths::profiles_dir);
    let profile = match (selected, dir) {
        (Some(game), Some(dir)) => read_shim_profile(&shim_profile_path(&dir, game)).ok(),
        _ => None,
    };
    SettingsExport::render(&ExportInput {
        gpu: &gpu,
        selected: selected_proton.as_ref(),
        protons: &discovery.installs,
        game: selected,
        profile: profile.as_ref(),
    })
}

/// One `name version · source · path` line for a Proton build.
fn proton_line(label: &str, install: &ProtonInstall) -> String {
    let version = install.version.as_deref().unwrap_or("(no version file)");
    format!(
        "{label:<13} {} ({version}) · {} · {}\n",
        install.name,
        install.source.label(),
        install.path.display()
    )
}

/// Whether an environment variable name looks like a credential.
fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_MARKERS.iter().any(|marker| upper.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use pitagames_core::Store;
    use pitagames_platform::compat::Version;
    use pitagames_platform::gpu::{GpuArch, GpuVendor, HdrSupport};
    use pitagames_platform::proton::{ProtonKind, ProtonSource};

    fn gpu() -> GpuProfile {
        GpuProfile {
            vendor: GpuVendor::Amd,
            arch: GpuArch::Rdna4,
            device_id: Some(0x7550),
            name: Some("AMD Radeon RX 9070".to_string()),
            mesa: Some(Version::parse("26.2").expect("a version")),
            vram_bytes: Some(16 * 1024 * 1024 * 1024),
            ntsync: true,
            hdr: HdrSupport::Supported,
            warnings: vec!["no vulkaninfo on PATH".to_string()],
        }
    }

    fn proton(name: &str, version: &str, source: ProtonSource) -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from(format!("/opt/proton/{name}")),
            name: name.to_string(),
            kind: ProtonKind::CachyOs,
            version: Some(version.to_string()),
            source,
        }
    }

    fn profile(env: &[(&str, &str)]) -> ShimProfile {
        let mut shim = ShimProfile::new(GameId::new(Store::Epic, "Gizmo"), "Bench");
        shim.env = env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        shim
    }

    #[test]
    fn the_export_names_the_gpu_proton_and_environment() {
        let selected = proton("proton-cachyos", "11.0-20260506", ProtonSource::Explicit);
        let other = proton("GE-Proton9-20", "9-20", ProtonSource::Umu);
        let game = GameId::new(Store::Epic, "Gizmo");
        let shim = profile(&[("DXVK_HUD", "fps"), ("GAMEID", "pitagames-epic-gizmo")]);

        let export = SettingsExport::render(&ExportInput {
            gpu: &gpu(),
            selected: Some(&selected),
            protons: &[selected.clone(), other],
            game: Some(&game),
            profile: Some(&shim),
        });
        let text = export.text();

        assert!(text.contains("AMD Radeon RX 9070"), "{text}");
        assert!(text.contains("rdna4"), "{text}");
        assert!(text.contains("16.0 GiB"), "{text}");
        assert!(text.contains("proton-cachyos"), "{text}");
        assert!(text.contains("GE-Proton9-20"), "{text}");
        assert!(text.contains("epic Gizmo"), "{text}");
        assert!(text.contains("DXVK_HUD=fps"), "{text}");
        assert!(text.contains("GAMEID=pitagames-epic-gizmo"), "{text}");
        // It is honest about being local.
        assert!(text.contains("no telemetry"), "{text}");
    }

    #[test]
    fn the_export_never_prints_a_credential() {
        let game = GameId::new(Store::Epic, "Gizmo");
        let shim = profile(&[
            ("DXVK_HUD", "fps"),
            ("PITAGAMES_STEAM_API_KEY", "deadbeef"),
            ("STORE_AUTH_TOKEN", "hunter2"),
            ("MY_PASSWORD", "letmein"),
            ("AUTH_CODE", "abc123"),
        ]);

        let export = SettingsExport::render(&ExportInput {
            gpu: &gpu(),
            selected: None,
            protons: &[],
            game: Some(&game),
            profile: Some(&shim),
        });
        let text = export.text();

        assert!(text.contains("DXVK_HUD=fps"), "{text}");
        for secret in ["deadbeef", "hunter2", "letmein", "abc123"] {
            assert!(!text.contains(secret), "leaked {secret}:\n{text}");
        }
        for name in [
            "PITAGAMES_STEAM_API_KEY",
            "STORE_AUTH_TOKEN",
            "MY_PASSWORD",
            "AUTH_CODE",
        ] {
            assert!(!text.contains(name), "leaked {name}:\n{text}");
        }
    }

    #[test]
    fn the_export_says_so_when_there_is_nothing_to_show() {
        let export = SettingsExport::render(&ExportInput {
            gpu: &gpu(),
            selected: None,
            protons: &[],
            game: None,
            profile: None,
        });
        assert!(export.text().contains("(none discovered)"));
        assert!(export.text().contains("(no profile"));
    }
}
