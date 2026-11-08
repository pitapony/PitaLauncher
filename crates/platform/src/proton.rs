//! Proton discovery and selection.
//!
//! PitaGames never installs or patches Proton; it finds what the machine
//! already has and picks one. Resolution order, highest priority first
//! (plan §5):
//!
//! 1. `proton-cachyos` from the distribution (`compatibilitytools.d`) — the
//!    recommended build on CachyOS because it carries the FSR 4 / OptiScaler
//!    environment hooks.
//! 2. User-installed `GE-Proton*` builds in Steam's `compatibilitytools.d`.
//! 3. Valve's own builds in `steamapps/common` (`Proton`, Experimental,
//!    Hotfix).
//! 4. umu-managed builds: `~/.local/share/umu/compatibilitytools` plus the
//!    `~/.local/share/Steam/compatibilitytools.d` copies umu downloads.
//!
//! Within one kind the newest build wins (numeric version comparison), and a
//! system build wins over a user copy of the same version.
//!
//! Discovery is read-only: it lists directories, follows symlinks, and reads
//! one small `version` file per candidate. It never starts Proton, writes to
//! Steam's config, or touches a game prefix.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use pitagames_core::ProtonRef;
use serde::{Deserialize, Serialize};

/// The launcher script every Proton build ships at its root.
const ENTRYPOINT: &str = "proton";

/// The small text file Valve writes next to [`ENTRYPOINT`].
const VERSION_FILE: &str = "version";

/// Longest `version` line we accept; anything longer is not a version string.
const VERSION_LINE_LIMIT: usize = 120;

/// Where a Proton build was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtonSource {
    /// A distribution package in `/usr/share/steam/compatibilitytools.d`
    /// (or `/usr/lib/steam/compatibilitytools.d`).
    SystemCompatibilityTools,
    /// A user-installed compatibility tool inside a Steam root.
    UserCompatibilityTools,
    /// One of Steam's own builds in `<steam>/steamapps/common`.
    SteamCommon,
    /// A build managed by umu-launcher.
    Umu,
    /// A directory the user pointed at directly (a [`ProtonRef::Path`]).
    Explicit,
}

impl ProtonSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::SystemCompatibilityTools => "system",
            Self::UserCompatibilityTools => "user",
            Self::SteamCommon => "steam",
            Self::Umu => "umu",
            Self::Explicit => "explicit",
        }
    }

    /// Tie-breaker inside one [`ProtonKind`]: lower wins.
    const fn rank(self) -> u8 {
        match self {
            Self::SystemCompatibilityTools => 0,
            Self::UserCompatibilityTools => 1,
            Self::SteamCommon => 2,
            Self::Umu => 3,
            Self::Explicit => 4,
        }
    }
}

/// Which family a build belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtonKind {
    /// `proton-cachyos` from the CachyOS repositories.
    CachyOs,
    /// GE-Proton (including umu's `GE-Latest` codename).
    Ge,
    /// Valve's regular Proton builds.
    Valve,
    /// Valve's `Proton - Experimental`.
    Experimental,
    /// UMU-Proton (Valve's stable branch plus the umu patch set).
    Umu,
    /// Anything else that still looks like a Proton build.
    Other,
}

impl ProtonKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::CachyOs => "proton-cachyos",
            Self::Ge => "GE-Proton",
            Self::Valve => "Steam Proton",
            Self::Experimental => "Proton Experimental",
            Self::Umu => "UMU-Proton",
            Self::Other => "other Proton build",
        }
    }

    /// Order in which [`ProtonRef::Auto`] prefers this kind; lower wins.
    const fn rank(self) -> u8 {
        match self {
            Self::CachyOs => 0,
            Self::Ge => 1,
            Self::Valve => 2,
            Self::Experimental => 3,
            Self::Umu => 4,
            Self::Other => 5,
        }
    }

    /// Classify a build from its directory name and `version` file.
    fn detect(name: &str, version: Option<&str>) -> Self {
        let name = name.to_ascii_lowercase();
        let version = version.unwrap_or_default().to_ascii_lowercase();
        let mentions = |needle: &str| name.contains(needle) || version.contains(needle);

        if mentions("cachy") {
            Self::CachyOs
        } else if mentions("ge-proton") || mentions("proton-ge") || mentions("ge-latest") {
            Self::Ge
        } else if mentions("experimental") {
            Self::Experimental
        } else if mentions("umu") {
            Self::Umu
        } else if mentions("proton") {
            Self::Valve
        } else {
            Self::Other
        }
    }
}

impl fmt::Display for ProtonKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// One directory to scan and the source it should be attributed to.
///
/// Roots are passed explicitly so tests (and the CLI) never depend on the
/// host: [`discover`] just builds them from [`default_roots`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtonRoot {
    pub path: PathBuf,
    pub source: ProtonSource,
}

impl ProtonRoot {
    pub fn new(path: impl Into<PathBuf>, source: ProtonSource) -> Self {
        Self {
            path: path.into(),
            source,
        }
    }
}

/// A Proton build found on disk.
///
/// `path` is the directory that contains the `proton` script, which is what
/// `PROTONPATH` (umu) and the GUI's "copy command" both need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtonInstall {
    pub path: PathBuf,
    /// Directory name — the stable identifier Steam and umu use.
    pub name: String,
    pub kind: ProtonKind,
    /// Contents of the `version` file, cleaned up; `None` when absent.
    pub version: Option<String>,
    pub source: ProtonSource,
}

impl ProtonInstall {
    /// Inspect one directory and describe it as a Proton build.
    ///
    /// `dir` may also be the `proton` entrypoint itself, so a file picker that
    /// hands back the executable still works.
    pub fn from_dir(dir: impl AsRef<Path>, source: ProtonSource) -> Result<Self, ProtonError> {
        let given = dir.as_ref();
        let path = resolve_dir(given)?;
        if !path.join(ENTRYPOINT).is_file() {
            return Err(ProtonError::NotProton {
                path: given.to_path_buf(),
            });
        }

        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let version = read_version(&path);
        let kind = ProtonKind::detect(&name, version.as_deref());

        Ok(Self {
            path,
            name,
            kind,
            version,
            source,
        })
    }

    /// The executable (a Python script in every Proton release) to run.
    pub fn entrypoint(&self) -> PathBuf {
        self.path.join(ENTRYPOINT)
    }

    /// The version string only when it says more than the directory name.
    ///
    /// Valve repeats the build name (`Proton 9.0` / `proton_9.0`), while
    /// proton-cachyos and umu append a build number worth showing.
    pub fn distinct_version(&self) -> Option<&str> {
        let version = self.version.as_deref()?;
        if normalize_name(version) == normalize_name(&self.name) {
            None
        } else {
            Some(version)
        }
    }

    /// Stable identity for de-duplication, resolving symlinked Steam roots.
    fn identity(&self) -> PathBuf {
        identity(&self.path)
    }

    /// Numeric components of the version, newest first when reversed.
    fn version_key(&self) -> Vec<u64> {
        numeric_runs(self.version.as_deref().unwrap_or(&self.name))
    }

    /// Selection order: kind first, then newest build, then source, then path.
    fn priority_cmp(&self, other: &Self) -> Ordering {
        self.kind
            .rank()
            .cmp(&other.kind.rank())
            .then_with(|| other.version_key().cmp(&self.version_key()))
            .then_with(|| self.source.rank().cmp(&other.source.rank()))
            .then_with(|| self.path.cmp(&other.path))
    }
}

/// Everything one discovery pass found.
///
/// `installs` is already in selection order: `installs[0]` is what
/// [`ProtonRef::Auto`] picks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub installs: Vec<ProtonInstall>,
    /// Every directory that was considered, in scan order.
    pub roots: Vec<ProtonRoot>,
    /// Unreadable roots and unexpected IO errors; missing roots are silent.
    pub warnings: Vec<String>,
}

impl DiscoveryReport {
    /// The build [`ProtonRef::Auto`] would choose.
    pub fn best(&self) -> Option<&ProtonInstall> {
        self.installs.first()
    }

    /// The newest build of one kind, if any.
    pub fn of_kind(&self, kind: ProtonKind) -> Option<&ProtonInstall> {
        self.installs.iter().find(|install| install.kind == kind)
    }

    /// Turn a profile's [`ProtonRef`] into a concrete build.
    ///
    /// `Ok(None)` means "Steam decides": the caller should leave the game's
    /// compatibility tool alone instead of passing a `PROTONPATH`.
    pub fn resolve(&self, reference: &ProtonRef) -> Result<Option<ProtonInstall>, ProtonError> {
        match reference {
            ProtonRef::Auto => self.best().cloned().map(Some).ok_or(ProtonError::NoneFound),
            ProtonRef::SteamDefault => Ok(None),
            ProtonRef::CachyOs => self.kind_or_error(ProtonKind::CachyOs),
            ProtonRef::GeProton => self.kind_or_error(ProtonKind::Ge),
            ProtonRef::Path { path } => {
                let wanted = identity(path);
                if let Some(found) = self
                    .installs
                    .iter()
                    .find(|install| install.identity() == wanted)
                {
                    return Ok(Some(found.clone()));
                }
                ProtonInstall::from_dir(path, ProtonSource::Explicit).map(Some)
            }
        }
    }

    fn kind_or_error(&self, kind: ProtonKind) -> Result<Option<ProtonInstall>, ProtonError> {
        self.of_kind(kind)
            .cloned()
            .map(Some)
            .ok_or(ProtonError::Unavailable { kind })
    }
}

/// Why Proton resolution failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtonError {
    #[error("no Proton installation found")]
    NoneFound,

    #[error("{kind} is not installed")]
    Unavailable { kind: ProtonKind },

    #[error("{} is not a Proton installation (no `proton` entrypoint)", path.display())]
    NotProton { path: PathBuf },
}

/// Discover Proton builds in the default locations on this machine.
pub fn discover() -> DiscoveryReport {
    discover_in(&default_roots())
}

/// Every location [`discover`] looks at, highest priority first.
///
/// Paths are returned even when they do not exist; discovery skips missing
/// directories silently, which is the normal case on a fresh system.
pub fn default_roots() -> Vec<ProtonRoot> {
    let umu_folders = std::env::var_os("UMU_FOLDERS_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    roots_from(
        &pitagames_core::paths::steam_roots(),
        pitagames_core::paths::data_home(),
        umu_folders,
    )
}

/// Discover inside explicit roots. The entry point used by tests and the CLI.
pub fn discover_in(roots: &[ProtonRoot]) -> DiscoveryReport {
    let mut report = DiscoveryReport {
        roots: roots.to_vec(),
        ..DiscoveryReport::default()
    };

    for root in roots {
        let entries = match fs::read_dir(&root.path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                report
                    .warnings
                    .push(format!("{}: {error}", root.path.display()));
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report
                        .warnings
                        .push(format!("{}: {error}", root.path.display()));
                    continue;
                }
            };

            let path = entry.path();
            // Follows symlinks: users commonly link a Proton build into
            // compatibilitytools.d instead of moving it.
            if !path.is_dir() {
                continue;
            }

            // `steamapps/common` holds every installed game; only Valve's own
            // builds are candidates there.
            if root.source == ProtonSource::SteamCommon
                && !entry
                    .file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .starts_with("proton")
            {
                continue;
            }

            match ProtonInstall::from_dir(&path, root.source) {
                Ok(install) => report.installs.push(install),
                // A half-extracted or locked download is normal; stay quiet.
                Err(ProtonError::NotProton { .. }) => {}
                Err(error) => report.warnings.push(error.to_string()),
            }
        }
    }

    report.installs.sort_by(ProtonInstall::priority_cmp);
    // Two Steam roots are usually the same directory (`~/.steam/steam` is a
    // symlink); keep the highest-priority copy after sorting.
    let mut seen = HashSet::new();
    report
        .installs
        .retain(|install| seen.insert(install.identity()));

    report
}

fn roots_from(
    steam_roots: &[PathBuf],
    data_home: Option<PathBuf>,
    umu_folders: Option<PathBuf>,
) -> Vec<ProtonRoot> {
    let mut roots = vec![
        ProtonRoot::new(
            "/usr/share/steam/compatibilitytools.d",
            ProtonSource::SystemCompatibilityTools,
        ),
        ProtonRoot::new(
            "/usr/lib/steam/compatibilitytools.d",
            ProtonSource::SystemCompatibilityTools,
        ),
    ];

    for steam in steam_roots {
        push_unique(
            &mut roots,
            ProtonRoot::new(
                steam.join("compatibilitytools.d"),
                ProtonSource::UserCompatibilityTools,
            ),
        );
        push_unique(
            &mut roots,
            ProtonRoot::new(steam.join("steamapps/common"), ProtonSource::SteamCommon),
        );
    }

    // umu-launcher's own layout, honouring its `UMU_FOLDERS_PATH` override.
    match umu_folders {
        Some(base) => {
            push_unique(
                &mut roots,
                ProtonRoot::new(base.join("umu/compatibilitytools"), ProtonSource::Umu),
            );
            push_unique(
                &mut roots,
                ProtonRoot::new(
                    base.join("Steam/compatibilitytools.d"),
                    ProtonSource::UserCompatibilityTools,
                ),
            );
        }
        None => {
            if let Some(data_home) = data_home {
                // GE-Proton downloaded on demand by umu lands in Steam's own
                // compatibilitytools.d, so it is attributed to the user.
                push_unique(
                    &mut roots,
                    ProtonRoot::new(
                        data_home.join("Steam/compatibilitytools.d"),
                        ProtonSource::UserCompatibilityTools,
                    ),
                );
                push_unique(
                    &mut roots,
                    ProtonRoot::new(data_home.join("umu/compatibilitytools"), ProtonSource::Umu),
                );
            }
        }
    }

    roots
}

fn push_unique(roots: &mut Vec<ProtonRoot>, root: ProtonRoot) {
    let wanted = identity(&root.path);
    if roots
        .iter()
        .any(|existing| identity(&existing.path) == wanted)
    {
        return;
    }
    roots.push(root);
}

/// Resolve symlinks when possible, otherwise keep the path as written.
fn identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_dir(path: &Path) -> Result<PathBuf, ProtonError> {
    let not_proton = || ProtonError::NotProton {
        path: path.to_path_buf(),
    };

    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(path.to_path_buf()),
        // A file picker may hand back `<dir>/proton` itself.
        Ok(_) if path.file_name() == Some(OsStr::new(ENTRYPOINT)) => {
            path.parent().map(Path::to_path_buf).ok_or_else(not_proton)
        }
        _ => Err(not_proton()),
    }
}

/// Read `<dir>/version`, dropping Valve's numeric build-id prefix.
fn read_version(dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(dir.join(VERSION_FILE)).ok()?;
    let line = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    if line.is_empty() || line.len() > VERSION_LINE_LIMIT {
        return None;
    }

    // Valve writes "1700757745 proton_9.0"; the timestamp is noise in a UI.
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    let rest: Vec<&str> = parts.collect();
    if !rest.is_empty() && first.chars().all(|c| c.is_ascii_digit()) {
        let rest = rest.join(" ");
        if rest.len() <= VERSION_LINE_LIMIT {
            return Some(rest);
        }
    }

    Some(line.to_string())
}

fn numeric_runs(text: &str) -> Vec<u64> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() {
            current.push(character);
        } else if !current.is_empty() {
            runs.push(current.parse().unwrap_or(0));
            current.clear();
        }
    }
    if !current.is_empty() {
        runs.push(current.parse().unwrap_or(0));
    }
    runs
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fake_proton(dir: &Path, version: Option<&str>) {
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::write(dir.join("proton"), "#!/usr/bin/env python3\n").unwrap();
        if let Some(version) = version {
            fs::write(dir.join("version"), version).unwrap();
        }
    }

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn discovers_and_orders_by_the_plan_priority() {
        let temp = temp();
        let system = temp.path().join("system");
        let user = temp.path().join("user");
        let steam_common = temp.path().join("steamapps/common");
        let umu = temp.path().join("umu");

        fake_proton(
            &system.join("proton-cachyos"),
            Some("proton-cachyos 10.0-20251101"),
        );
        fake_proton(&user.join("GE-Proton9-20"), Some("GE-Proton9-20"));
        fake_proton(
            &steam_common.join("Proton - Experimental"),
            Some("experimental-9.0-20250101"),
        );
        fake_proton(&umu.join("UMU-Proton"), Some("UMU-Proton"));

        let roots = [
            ProtonRoot::new(&system, ProtonSource::SystemCompatibilityTools),
            ProtonRoot::new(&user, ProtonSource::UserCompatibilityTools),
            ProtonRoot::new(&steam_common, ProtonSource::SteamCommon),
            ProtonRoot::new(&umu, ProtonSource::Umu),
        ];
        let report = discover_in(&roots);

        let names: Vec<&str> = report
            .installs
            .iter()
            .map(|install| install.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "proton-cachyos",
                "GE-Proton9-20",
                "Proton - Experimental",
                "UMU-Proton",
            ]
        );
        assert_eq!(report.best().unwrap().kind, ProtonKind::CachyOs);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn steam_common_only_offers_proton_directories() {
        let temp = temp();
        let common = temp.path().join("steamapps/common");
        fake_proton(&common.join("Proton 9.0"), Some("1700000000 proton_9.0"));
        // A game folder that happens to contain a script named `proton` must
        // never be offered as a runtime.
        fake_proton(&common.join("Portal 2"), Some("not a runtime"));

        let report = discover_in(&[ProtonRoot::new(&common, ProtonSource::SteamCommon)]);
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.installs[0].name, "Proton 9.0");
    }

    #[test]
    fn newer_builds_win_within_a_kind() {
        let temp = temp();
        let user = temp.path().join("compat");
        for name in ["GE-Proton9-3", "GE-Proton9-20", "GE-Proton9-9"] {
            fake_proton(&user.join(name), None);
        }

        let report = discover_in(&[ProtonRoot::new(&user, ProtonSource::UserCompatibilityTools)]);
        let names: Vec<&str> = report
            .installs
            .iter()
            .map(|install| install.name.as_str())
            .collect();
        assert_eq!(names, ["GE-Proton9-20", "GE-Proton9-9", "GE-Proton9-3"]);
    }

    #[test]
    fn the_version_file_beats_the_directory_name_for_ordering() {
        let temp = temp();
        let user = temp.path().join("compat");
        fake_proton(&user.join("GE-Latest"), Some("GE-Proton10-1"));
        fake_proton(&user.join("GE-Proton9-20"), Some("GE-Proton9-20"));

        let report = discover_in(&[ProtonRoot::new(&user, ProtonSource::UserCompatibilityTools)]);
        assert_eq!(report.installs[0].name, "GE-Latest");
        assert_eq!(report.installs[0].kind, ProtonKind::Ge);
    }

    #[test]
    fn system_builds_win_ties_over_user_copies() {
        let temp = temp();
        let system = temp.path().join("system");
        let user = temp.path().join("user");
        fake_proton(&system.join("GE-Proton9-20"), None);
        fake_proton(&user.join("GE-Proton9-20"), None);

        let report = discover_in(&[
            ProtonRoot::new(&user, ProtonSource::UserCompatibilityTools),
            ProtonRoot::new(&system, ProtonSource::SystemCompatibilityTools),
        ]);
        assert_eq!(
            report.installs.len(),
            2,
            "different paths are not duplicates"
        );
        assert_eq!(
            report.installs[0].source,
            ProtonSource::SystemCompatibilityTools
        );
        assert_eq!(
            report.installs[1].source,
            ProtonSource::UserCompatibilityTools
        );
    }

    #[test]
    fn duplicate_and_symlinked_roots_are_deduplicated() {
        let temp = temp();
        let compat = temp.path().join("Steam/compatibilitytools.d");
        fake_proton(&compat.join("GE-Proton9-20"), None);

        let link = temp.path().join("steam-link");
        symlink(&compat, &link).unwrap();

        let report = discover_in(&[
            ProtonRoot::new(&compat, ProtonSource::UserCompatibilityTools),
            ProtonRoot::new(&link, ProtonSource::UserCompatibilityTools),
            ProtonRoot::new(&compat, ProtonSource::UserCompatibilityTools),
        ]);
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.installs[0].name, "GE-Proton9-20");
    }

    #[test]
    fn symlinked_installs_are_found() {
        let temp = temp();
        let outside = temp.path().join("outside/GE-Proton10-5");
        fake_proton(&outside, None);
        let compat = temp.path().join("compatibilitytools.d");
        fs::create_dir_all(&compat).unwrap();
        symlink(&outside, compat.join("GE-Proton10-5")).unwrap();

        let report = discover_in(&[ProtonRoot::new(
            &compat,
            ProtonSource::UserCompatibilityTools,
        )]);
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.installs[0].kind, ProtonKind::Ge);
    }

    #[test]
    fn missing_roots_are_silent_and_a_file_as_root_warns() {
        let temp = temp();
        let missing = temp.path().join("does-not-exist");
        let file = temp.path().join("not-a-directory");
        fs::write(&file, "x").unwrap();

        let report = discover_in(&[
            ProtonRoot::new(&missing, ProtonSource::UserCompatibilityTools),
            ProtonRoot::new(&file, ProtonSource::UserCompatibilityTools),
        ]);
        assert!(report.installs.is_empty());
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("not-a-directory"));
    }

    #[test]
    fn directories_without_the_entrypoint_are_skipped() {
        let temp = temp();
        let user = temp.path().join("compat");
        fs::create_dir_all(user.join("GE-Proton9-20.7z.part")).unwrap();
        fs::create_dir_all(user.join("GE-Proton9-20")).unwrap();
        // A directory with a version file but no `proton` script is not an
        // installation.
        fs::write(user.join("GE-Proton9-20").join("version"), "GE-Proton9-20").unwrap();

        let report = discover_in(&[ProtonRoot::new(&user, ProtonSource::UserCompatibilityTools)]);
        assert!(report.installs.is_empty());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn version_file_formats() {
        let temp = temp();
        let dir = temp.path();

        fs::write(dir.join("version"), "1700757745 proton_9.0\n").unwrap();
        assert_eq!(read_version(dir).as_deref(), Some("proton_9.0"));

        fs::write(dir.join("version"), "GE-Proton9-20\n\n").unwrap();
        assert_eq!(read_version(dir).as_deref(), Some("GE-Proton9-20"));

        fs::write(dir.join("version"), "proton-cachyos 10.0-20251101\n").unwrap();
        assert_eq!(
            read_version(dir).as_deref(),
            Some("proton-cachyos 10.0-20251101")
        );

        fs::write(dir.join("version"), "\n \n").unwrap();
        assert_eq!(read_version(dir), None);

        fs::write(dir.join("version"), "x".repeat(VERSION_LINE_LIMIT + 1)).unwrap();
        assert_eq!(read_version(dir), None);

        fs::remove_file(dir.join("version")).unwrap();
        assert_eq!(read_version(dir), None);
    }

    #[test]
    fn kinds_are_detected_from_names_and_versions() {
        let cases = [
            ("proton-cachyos", ProtonKind::CachyOs),
            ("proton-cachyos-slr", ProtonKind::CachyOs),
            ("GE-Proton9-20", ProtonKind::Ge),
            ("GE-Latest", ProtonKind::Ge),
            ("Proton-GE-8-30", ProtonKind::Ge),
            ("Proton - Experimental", ProtonKind::Experimental),
            ("UMU-Proton", ProtonKind::Umu),
            ("Proton 9.0", ProtonKind::Valve),
            ("Something Else", ProtonKind::Other),
        ];
        for (name, expected) in cases {
            assert_eq!(ProtonKind::detect(name, None), expected, "{name}");
        }

        assert_eq!(
            ProtonKind::detect("custom-build", Some("proton-cachyos 10.0")),
            ProtonKind::CachyOs
        );
        assert_eq!(
            ProtonKind::detect("Proton", Some("1700757745 experimental-9.0-20250101")),
            ProtonKind::Experimental
        );
    }

    #[test]
    fn distinct_version_hides_repeated_build_names() {
        let temp = temp();
        let ge = temp.path().join("GE-Proton9-20");
        fake_proton(&ge, Some("GE-Proton9-20"));
        let install = ProtonInstall::from_dir(&ge, ProtonSource::UserCompatibilityTools).unwrap();
        assert_eq!(install.distinct_version(), None);

        let valve = temp.path().join("Proton 9.0");
        fake_proton(&valve, Some("proton_9.0"));
        let install = ProtonInstall::from_dir(&valve, ProtonSource::SteamCommon).unwrap();
        assert_eq!(install.distinct_version(), None);

        let cachy = temp.path().join("proton-cachyos");
        fake_proton(&cachy, Some("proton-cachyos 10.0-20251101"));
        let install =
            ProtonInstall::from_dir(&cachy, ProtonSource::SystemCompatibilityTools).unwrap();
        assert_eq!(
            install.distinct_version(),
            Some("proton-cachyos 10.0-20251101")
        );
    }

    #[test]
    fn resolution_honours_auto_steam_default_kinds_and_paths() {
        let temp = temp();
        let system = temp.path().join("system");
        let user = temp.path().join("user");
        let cachy = system.join("proton-cachyos");
        let ge = user.join("GE-Proton9-20");
        fake_proton(&cachy, None);
        fake_proton(&ge, None);

        let report = discover_in(&[
            ProtonRoot::new(&system, ProtonSource::SystemCompatibilityTools),
            ProtonRoot::new(&user, ProtonSource::UserCompatibilityTools),
        ]);

        assert_eq!(
            report.resolve(&ProtonRef::Auto).unwrap().unwrap().path,
            cachy
        );
        assert!(report.resolve(&ProtonRef::SteamDefault).unwrap().is_none());
        assert_eq!(
            report.resolve(&ProtonRef::CachyOs).unwrap().unwrap().path,
            cachy
        );
        assert_eq!(
            report.resolve(&ProtonRef::GeProton).unwrap().unwrap().path,
            ge
        );

        // A path may be the directory or the entrypoint itself.
        let by_dir = report
            .resolve(&ProtonRef::Path { path: ge.clone() })
            .unwrap()
            .unwrap();
        assert_eq!(by_dir.path, ge);
        assert_eq!(by_dir.source, ProtonSource::UserCompatibilityTools);

        let by_script = report
            .resolve(&ProtonRef::Path {
                path: ge.join("proton"),
            })
            .unwrap()
            .unwrap();
        assert_eq!(by_script.path, ge);
        assert_eq!(by_script.source, ProtonSource::Explicit);

        assert!(matches!(
            report.resolve(&ProtonRef::Path {
                path: temp.path().join("nope"),
            }),
            Err(ProtonError::NotProton { .. })
        ));
    }

    #[test]
    fn unavailable_kinds_and_empty_discovery_report_errors() {
        let report = DiscoveryReport::default();
        assert!(matches!(
            report.resolve(&ProtonRef::Auto),
            Err(ProtonError::NoneFound)
        ));

        let error = report.resolve(&ProtonRef::GeProton).unwrap_err();
        assert!(error.to_string().contains("GE-Proton"), "{error}");
    }

    #[test]
    fn root_plan_covers_every_documented_location() {
        let temp = temp();
        let steam = temp.path().join("Steam");
        let data = temp.path().join("data");

        let roots = roots_from(std::slice::from_ref(&steam), Some(data.clone()), None);
        let paths: Vec<&Path> = roots.iter().map(|root| root.path.as_path()).collect();

        assert!(paths.contains(&Path::new("/usr/share/steam/compatibilitytools.d")));
        assert!(paths.contains(&Path::new("/usr/lib/steam/compatibilitytools.d")));
        assert!(paths.contains(&steam.join("compatibilitytools.d").as_path()));
        assert!(paths.contains(&steam.join("steamapps/common").as_path()));
        assert!(paths.contains(&data.join("umu/compatibilitytools").as_path()));
        assert!(paths.contains(&data.join("Steam/compatibilitytools.d").as_path()));

        let sources: Vec<ProtonSource> = roots.iter().map(|root| root.source).collect();
        assert!(sources.contains(&ProtonSource::SystemCompatibilityTools));
        assert!(sources.contains(&ProtonSource::Umu));
    }

    #[test]
    fn umu_folders_path_replaces_the_data_home_defaults() {
        let temp = temp();
        let folders = temp.path().join("home");
        let roots = roots_from(&[], None, Some(folders.clone()));

        assert!(roots.iter().any(|root| {
            root.path == folders.join("umu/compatibilitytools") && root.source == ProtonSource::Umu
        }));
        assert!(roots.iter().any(|root| {
            root.path == folders.join("Steam/compatibilitytools.d")
                && root.source == ProtonSource::UserCompatibilityTools
        }));
    }

    #[test]
    fn installs_round_trip_through_json_for_the_cli() {
        let temp = temp();
        let dir = temp.path().join("proton-cachyos");
        fake_proton(&dir, Some("proton-cachyos 10.0-20251101"));
        let install =
            ProtonInstall::from_dir(&dir, ProtonSource::SystemCompatibilityTools).unwrap();

        let json = serde_json::to_string(&install).unwrap();
        assert!(json.contains(r#""kind":"cachy_os""#), "{json}");
        assert!(
            json.contains(r#""source":"system_compatibility_tools""#),
            "{json}"
        );
        let parsed: ProtonInstall = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, install);
    }
}
