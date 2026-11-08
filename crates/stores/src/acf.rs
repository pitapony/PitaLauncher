//! Parser for Steam's `appmanifest_<appid>.acf` files.

use std::fs;
use std::path::Path;

use time::OffsetDateTime;

use crate::Error;
use crate::vdf;

/// The subset of an `AppState` manifest that the launcher needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifest {
    pub app_id: u32,
    pub name: String,
    pub install_dir: Option<String>,
    pub size_on_disk: Option<u64>,
    pub build_id: Option<String>,
    pub last_updated: Option<OffsetDateTime>,
    pub state_flags: Option<u32>,
    pub installed: bool,
    /// Bytes already fetched for the current update/install, when Steam
    /// records them (not every phase writes these keys).
    pub bytes_downloaded: Option<u64>,
    /// Total bytes for the current update/install, when recorded.
    pub bytes_to_download: Option<u64>,
}

/// `StateFlags` bits used by Steam's app manifests.
///
/// Values match SteamKit's `AppStateFlags`; the manifest stores the bits as a
/// decimal integer. Bits are cumulative, so predicates must mask rather than
/// compare equality.
pub mod flags {
    pub const INVALID: u32 = 1 << 0;
    pub const UNINSTALLED: u32 = 1 << 1;
    pub const FULLY_INSTALLED: u32 = 1 << 2;
    pub const UPDATE_REQUIRED: u32 = 1 << 3;
    pub const UPDATE_QUEUED: u32 = 1 << 4;
    pub const UPDATE_OPTIONAL: u32 = 1 << 5;
    pub const FILES_MISSING: u32 = 1 << 6;
    pub const SHARED_ONLY: u32 = 1 << 7;
    pub const FILES_CORRUPT: u32 = 1 << 8;
    pub const UPDATE_RUNNING: u32 = 1 << 9;
    pub const UPDATE_PAUSED: u32 = 1 << 10;
    pub const UPDATE_STARTED: u32 = 1 << 11;
    pub const UNINSTALLING: u32 = 1 << 12;
    pub const BACKUP_RUNNING: u32 = 1 << 13;
    pub const RECONFIGURING: u32 = 1 << 14;
    pub const VALIDATING: u32 = 1 << 15;
    pub const ADDING_FILES: u32 = 1 << 16;
    pub const PREALLOCATING: u32 = 1 << 17;
    pub const DOWNLOADING: u32 = 1 << 18;
    pub const STAGING: u32 = 1 << 19;
    pub const COMMITTING: u32 = 1 << 20;
    pub const UPDATE_STOPPING: u32 = 1 << 21;
}

/// A user-facing phase derived from `StateFlags`.
///
/// `UpdateAvailable` means "the installed copy is usable, but Steam has a
/// newer build queued". Busy phases mean the install must not be launched (and
/// the Downloads view should show progress).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Unknown,
    NotInstalled,
    Installed,
    UpdateAvailable,
    Downloading,
    Updating,
    Validating,
    Uninstalling,
}

impl InstallPhase {
    fn from_flags(flags: u32) -> Self {
        if flags & flags::INVALID != 0 {
            return Self::Unknown;
        }
        if flags & flags::UNINSTALLED != 0 {
            return Self::NotInstalled;
        }
        if flags & flags::UNINSTALLING != 0 {
            return Self::Uninstalling;
        }
        if flags & (flags::DOWNLOADING | flags::STAGING) != 0 {
            return Self::Downloading;
        }
        if flags
            & (flags::UPDATE_RUNNING
                | flags::UPDATE_STARTED
                | flags::COMMITTING
                | flags::PREALLOCATING
                | flags::ADDING_FILES
                | flags::RECONFIGURING
                | flags::BACKUP_RUNNING)
            != 0
        {
            return Self::Updating;
        }
        if flags & flags::VALIDATING != 0 {
            return Self::Validating;
        }
        if flags
            & (flags::UPDATE_REQUIRED
                | flags::UPDATE_QUEUED
                | flags::UPDATE_OPTIONAL
                | flags::UPDATE_PAUSED
                | flags::UPDATE_STOPPING
                | flags::FILES_MISSING
                | flags::FILES_CORRUPT)
            != 0
        {
            return Self::UpdateAvailable;
        }
        if flags & flags::FULLY_INSTALLED != 0 {
            return Self::Installed;
        }
        Self::Unknown
    }

    /// The game's files are present (possibly with an update pending).
    pub const fn is_installed(self) -> bool {
        matches!(
            self,
            Self::Installed
                | Self::UpdateAvailable
                | Self::Downloading
                | Self::Updating
                | Self::Validating
        )
    }

    /// Steam is touching the files; do not launch the game.
    pub const fn is_busy(self) -> bool {
        matches!(
            self,
            Self::Downloading | Self::Updating | Self::Validating | Self::Uninstalling
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotInstalled => "not installed",
            Self::Installed => "installed",
            Self::UpdateAvailable => "update available",
            Self::Downloading => "downloading",
            Self::Updating => "updating",
            Self::Validating => "validating",
            Self::Uninstalling => "uninstalling",
        }
    }
}

/// Best-effort download progress from the manifest's byte counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: u64,
}

impl DownloadProgress {
    /// `None` when Steam has not written a total (indeterminate progress).
    pub fn fraction(self) -> Option<f32> {
        (self.total > 0).then(|| (self.downloaded as f32 / self.total as f32).clamp(0.0, 1.0))
    }

    pub fn percent(self) -> Option<u8> {
        self.fraction()
            .map(|fraction| (fraction * 100.0).round() as u8)
    }
}

impl AppManifest {
    pub fn parse_file(path: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Self::parse_str(&text).map_err(|source| Error::Manifest {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn parse_str(input: &str) -> Result<Self, ManifestError> {
        let root = vdf::parse(input)?;
        let state = root
            .get_object("AppState")
            .ok_or(ManifestError::MissingRoot)?;

        let app_id = state
            .get_str("appid")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .ok_or(ManifestError::MissingAppId)?;

        let name = state
            .get_str("name")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("App {app_id}"));

        let install_dir = state
            .get_str("installdir")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        let state_flags = parse_u32(state.get_str("StateFlags"));
        // Missing flags means the manifest exists for an installed app; an
        // explicit flag word is classified, with update bits still counting as
        // "files present".
        let installed =
            state_flags.is_none_or(|bits| InstallPhase::from_flags(bits).is_installed());

        Ok(Self {
            app_id,
            name,
            install_dir,
            size_on_disk: parse_u64(state.get_str("SizeOnDisk")),
            build_id: state
                .get_str("buildid")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            last_updated: state
                .get_str("LastUpdated")
                .and_then(|value| value.trim().parse::<i64>().ok())
                .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok()),
            state_flags,
            installed,
            bytes_downloaded: parse_u64(state.get_str("BytesDownloaded")),
            bytes_to_download: parse_u64(state.get_str("BytesToDownload")),
        })
    }

    /// The phase derived from `StateFlags`; missing flags mean installed.
    pub fn phase(&self) -> InstallPhase {
        match self.state_flags {
            Some(flags) => InstallPhase::from_flags(flags),
            None => InstallPhase::Installed,
        }
    }

    /// Byte counters for the current install/update, when present.
    pub fn download_progress(&self) -> Option<DownloadProgress> {
        Some(DownloadProgress {
            downloaded: self.bytes_downloaded?,
            total: self.bytes_to_download?,
        })
    }

    /// Installed and not being written: safe to launch and terminal for an
    /// install poll.
    pub fn is_complete(&self) -> bool {
        self.installed && !self.phase().is_busy()
    }
}

fn parse_u32(value: Option<&str>) -> Option<u32> {
    value?.trim().parse().ok()
}

fn parse_u64(value: Option<&str>) -> Option<u64> {
    value?.trim().parse().ok()
}

/// Why a manifest could not be turned into an [`AppManifest`].
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error(transparent)]
    Vdf(#[from] vdf::ParseError),

    #[error("missing AppState object")]
    MissingRoot,

    #[error("missing appid")]
    MissingAppId,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTAL_2: &str = r#"
"AppState"
{
    "appid"        "620"
    "name"         "Portal 2"
    "StateFlags"   "4"
    "installdir"   "Portal 2"
    "LastUpdated"  "1735689600"
    "SizeOnDisk"   "12884901888"
    "buildid"      "9876543"
}
"#;

    #[test]
    fn parses_installed_manifest() {
        let manifest = AppManifest::parse_str(PORTAL_2).unwrap();
        assert_eq!(manifest.app_id, 620);
        assert_eq!(manifest.name, "Portal 2");
        assert_eq!(manifest.install_dir.as_deref(), Some("Portal 2"));
        assert_eq!(manifest.size_on_disk, Some(12_884_901_888));
        assert_eq!(manifest.build_id.as_deref(), Some("9876543"));
        assert!(manifest.installed);
        assert!(manifest.last_updated.is_some());
    }

    #[test]
    fn state_flags_without_bit_4_is_not_installed() {
        let text = PORTAL_2.replace("\"StateFlags\"   \"4\"", "\"StateFlags\"   \"2\"");
        assert!(!AppManifest::parse_str(&text).unwrap().installed);
    }

    #[test]
    fn missing_state_flags_is_treated_as_installed() {
        let text = PORTAL_2.replace("\"StateFlags\"   \"4\"\n", "");
        assert!(AppManifest::parse_str(&text).unwrap().installed);
    }

    #[test]
    fn maps_state_flags_to_phases() {
        use InstallPhase::*;

        let cases = [
            (flags::FULLY_INSTALLED, Installed),
            (flags::UPDATE_REQUIRED, UpdateAvailable),
            (
                flags::UPDATE_QUEUED | flags::FULLY_INSTALLED,
                UpdateAvailable,
            ),
            (flags::UPDATE_PAUSED, UpdateAvailable),
            (flags::FILES_CORRUPT, UpdateAvailable),
            (flags::DOWNLOADING, Downloading),
            (flags::STAGING, Downloading),
            (flags::UPDATE_RUNNING, Updating),
            (flags::PREALLOCATING, Updating),
            (flags::VALIDATING, Validating),
            (flags::UNINSTALLING, Uninstalling),
            (flags::UNINSTALLED, NotInstalled),
            (flags::INVALID, Unknown),
            (0, Unknown),
        ];

        for (bits, expected) in cases {
            assert_eq!(InstallPhase::from_flags(bits), expected, "flags {bits}");
        }
    }

    #[test]
    fn update_required_is_installed_but_not_busy() {
        let text = PORTAL_2.replace("\"StateFlags\"   \"4\"", "\"StateFlags\"   \"8\"");
        let manifest = AppManifest::parse_str(&text).unwrap();

        assert!(manifest.installed);
        assert_eq!(manifest.phase(), InstallPhase::UpdateAvailable);
        assert!(!manifest.phase().is_busy());
        assert!(manifest.is_complete());
    }

    #[test]
    fn parsing_byte_counters_reports_progress() {
        let text = PORTAL_2
            .replace("\"StateFlags\"   \"4\"", "\"StateFlags\"   \"262144\"")
            .replace(
                "\"buildid\"      \"9876543\"",
                "\"buildid\"      \"9876543\"\n    \"BytesDownloaded\" \"1000\"\n    \"BytesToDownload\" \"4000\"",
            );
        let manifest = AppManifest::parse_str(&text).unwrap();

        assert_eq!(manifest.bytes_downloaded, Some(1_000));
        assert_eq!(manifest.bytes_to_download, Some(4_000));
        let progress = manifest.download_progress().unwrap();
        assert_eq!(progress.fraction(), Some(0.25));
        assert_eq!(progress.percent(), Some(25));
        assert_eq!(manifest.phase(), InstallPhase::Downloading);
        assert!(manifest.installed);
        assert!(!manifest.is_complete());
    }

    #[test]
    fn missing_byte_counters_are_indeterminate() {
        let manifest = AppManifest::parse_str(PORTAL_2).unwrap();
        assert!(manifest.download_progress().is_none());

        let zero = DownloadProgress {
            downloaded: 1,
            total: 0,
        };
        assert_eq!(zero.fraction(), None);
        assert_eq!(zero.percent(), None);
    }

    #[test]
    fn rejects_documents_without_app_state() {
        let error = AppManifest::parse_str(r#""other" { "appid" "1" }"#).unwrap_err();
        assert!(matches!(error, ManifestError::MissingRoot));
    }

    #[test]
    fn rejects_manifests_without_appid() {
        let error = AppManifest::parse_str(r#""AppState" { "name" "X" }"#).unwrap_err();
        assert!(matches!(error, ManifestError::MissingAppId));
    }
}
