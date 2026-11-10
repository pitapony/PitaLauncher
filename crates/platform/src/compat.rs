//! The shipped per-game compatibility database.
//!
//! PitaGames ships a curated TOML database — `compat/*.toml` — that records what
//! a game's upscaling actually supports, which option to recommend for each GPU
//! architecture, the preferred injection DLL, known runtime issues and whether
//! DLSSNR neural rendering is available. The graphics layer (M3) reads it; this
//! module is the loader, the merge and the validation.
//!
//! **Two layers, merged field by field.**
//!
//! - *Shipped* entries live in `/usr/share/pitagames/compat` and a synced copy
//!   under `$XDG_DATA_HOME/pitagames/compat`. They are the baseline.
//! - *User* entries live in `$XDG_CONFIG_HOME/pitagames/compat`. A user file with
//!   the same `[game] store`/`store_id` as a shipped entry overrides only the
//!   fields it sets; a game the shipped DB does not know is added.
//!
//! Every file is one entry with a top-level `[game]` table (the groups
//! `[upscaling]`, `[runtime]` and `[neural_rendering]` are optional). The game
//! identity always comes from the file's content, never from its name, so a
//! user override names the game with `store`/`store_id` and then sets only the
//! fields it wants to change — the rest is inherited from the shipped entry.
//!
//! **Validation is a first-class output.** A bad shipped file is a curation bug
//! and a bad user file is a broken override, but neither may stop the launcher
//! from starting: parse and validation problems are collected as warnings. The
//! CLI prints them, and a test loads the repository's real `compat/` directory
//! and asserts it is warning-free.
//!
//! The loader is framework-free and DB-unaware, matching `platform::proton`:
//! explicit directories in (so fixtures and a second checkout work), a report
//! out.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use pitagames_core::profile::{InjectionDll, NrBackend, NrRoute, Upscaler};
use pitagames_core::{AntiCheat, GameId, Store};

/// Env var that replaces the shipped compat directories with one path.
pub const COMPAT_DIR_ENV: &str = "PITAGAMES_COMPAT_DIR";

/// The installed location of the shipped database.
pub const SYSTEM_COMPAT_DIR: &str = "/usr/share/pitagames/compat";

/// Neural-rendering `model_scale` bounds, shared with `core::profile`.
const MODEL_SCALE_MIN: u8 = 25;
const MODEL_SCALE_MAX: u8 = 100;

// ---------------------------------------------------------------------------
// Architecture
// ---------------------------------------------------------------------------

/// A GPU architecture family the database can recommend for.
///
/// These are capability buckets, not exact cards: `Rdna4` covers the RX 9000
/// series (our validated target), `Rtx` and `Arc` are modelled but shipped as
/// `experimental` because we have no in-house hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    Rdna4,
    Rdna3,
    Rdna2,
    Rdna1,
    Rtx,
    Arc,
}

impl Arch {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rdna4 => "rdna4",
            Self::Rdna3 => "rdna3",
            Self::Rdna2 => "rdna2",
            Self::Rdna1 => "rdna1",
            Self::Rtx => "rtx",
            Self::Arc => "arc",
        }
    }

    /// The human label the UI shows next to a recommendation.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Rdna4 => "RDNA 4 (RX 9000)",
            Self::Rdna3 => "RDNA 3 (RX 7000)",
            Self::Rdna2 => "RDNA 2 (RX 6000)",
            Self::Rdna1 => "RDNA 1 (RX 5000)",
            Self::Rtx => "NVIDIA RTX",
            Self::Arc => "Intel Arc",
        }
    }

    /// The architectures we can validate in-house, per the plan's hardware matrix.
    pub const fn is_in_house(self) -> bool {
        matches!(self, Self::Rdna4 | Self::Rdna1)
    }
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

/// A `major.minor[.patch]` version, used for the Mesa and DLSSNR gates.
///
/// Comparisons ignore a missing patch (treated as `0`), so `26.2` and `26.2.0`
/// order the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parse `"26.2"`, `"26.2.3"` or `"0.0.8"`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = parts.next()?.trim().parse().ok()?;
        let minor = parts.next()?.trim().parse().ok()?;
        let patch = match parts.next() {
            Some(patch) => patch.trim().parse().ok()?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.patch == 0 {
            write!(formatter, "{}.{}", self.major, self.minor)
        } else {
            write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
        }
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Version::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("`{text}` is not a major.minor[.patch] version"))
        })
    }
}

// ---------------------------------------------------------------------------
// Upscaler identities
// ---------------------------------------------------------------------------

/// What a game's own settings can select.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameApi {
    Dlss,
    Fsr2,
    Fsr3,
    Fsr31,
    Fsr4,
    Xess,
}

impl GameApi {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dlss => "dlss",
            Self::Fsr2 => "fsr2",
            Self::Fsr3 => "fsr3",
            Self::Fsr31 => "fsr31",
            Self::Fsr4 => "fsr4",
            Self::Xess => "xess",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Dlss => "DLSS",
            Self::Fsr2 => "FSR 2",
            Self::Fsr3 => "FSR 3",
            Self::Fsr31 => "FSR 3.1",
            Self::Fsr4 => "FSR 4",
            Self::Xess => "XeSS",
        }
    }
}

/// The upscaler the database recommends for an architecture.
///
/// A plain-string identity so the TOML stays readable. [`Self::to_upscaler`]
/// turns it into the launch-profile enum; `DlssNative` has no injection, it
/// means "leave the game's own DLSS on" (the RTX path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpscalerId {
    Native,
    DlssNative,
    Fsr4Upgrade,
    Fsr4Rdna3,
    DlssToFsr4,
    DlssToFsr31,
    DlssToXess,
    XessUpgrade,
    Ffx3Upgrade,
    Ffx4Upgrade,
    HlxDlss,
    HlxFsr4,
    HlxFsr4DropIn,
    SystemFsr,
}

impl UpscalerId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::DlssNative => "dlss_native",
            Self::Fsr4Upgrade => "fsr4_upgrade",
            Self::Fsr4Rdna3 => "fsr4_rdna3",
            Self::DlssToFsr4 => "dlss_to_fsr4",
            Self::DlssToFsr31 => "dlss_to_fsr31",
            Self::DlssToXess => "dlss_to_xess",
            Self::XessUpgrade => "xess_upgrade",
            Self::Ffx3Upgrade => "ffx3_upgrade",
            Self::Ffx4Upgrade => "ffx4_upgrade",
            Self::HlxDlss => "hlx_dlss",
            Self::HlxFsr4 => "hlx_fsr4",
            Self::HlxFsr4DropIn => "hlx_fsr4_drop_in",
            Self::SystemFsr => "system_fsr",
        }
    }

    /// Map the recommendation onto the launch profile's upscaler choice.
    pub const fn to_upscaler(self) -> Upscaler {
        match self {
            // The game's own upscaler is left alone; `DlssNative` is the RTX
            // case where the game already runs DLSS.
            Self::Native | Self::DlssNative => Upscaler::Native,
            Self::Fsr4Upgrade => Upscaler::Fsr4Upgrade { version: None },
            Self::Fsr4Rdna3 => Upscaler::Fsr4Rdna3 { version: None },
            Self::DlssToFsr4 => Upscaler::DlssToFsr4,
            Self::DlssToFsr31 => Upscaler::DlssToFsr31,
            Self::DlssToXess => Upscaler::DlssToXess,
            Self::XessUpgrade => Upscaler::XessUpgrade,
            Self::Ffx3Upgrade => Upscaler::Ffx3Upgrade,
            Self::Ffx4Upgrade => Upscaler::Ffx4Upgrade,
            Self::HlxDlss => Upscaler::HlxDlss,
            Self::HlxFsr4 => Upscaler::HlxFsr4,
            Self::HlxFsr4DropIn => Upscaler::HlxFsr4DropIn,
            Self::SystemFsr => Upscaler::SystemFsr,
        }
    }
}

/// The DLL a manual OptiScaler deployment prefers for a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PreferredDll {
    #[serde(rename = "dxgi.dll", alias = "dxgi")]
    Dxgi,
    #[serde(rename = "d3d12.dll", alias = "d3d12")]
    D3d12,
    #[serde(rename = "dbghelp.dll", alias = "dbghelp")]
    Dbghelp,
    #[serde(rename = "winmm.dll", alias = "winmm")]
    Winmm,
    #[serde(rename = "version.dll", alias = "version")]
    Version,
}

impl PreferredDll {
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Dxgi => "dxgi.dll",
            Self::D3d12 => "d3d12.dll",
            Self::Dbghelp => "dbghelp.dll",
            Self::Winmm => "winmm.dll",
            Self::Version => "version.dll",
        }
    }

    pub const fn to_injection_dll(self) -> InjectionDll {
        match self {
            Self::Dxgi => InjectionDll::Dxgi,
            Self::D3d12 => InjectionDll::D3d12,
            Self::Dbghelp => InjectionDll::Dbghelp,
            Self::Winmm => InjectionDll::Winmm,
            Self::Version => InjectionDll::Version,
        }
    }
}

/// Which HelixSR product fits a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelixsrMode {
    /// The OptiScaler-based Scaler (HLX DLSS / HLX FSR 4).
    Scaler,
    /// The FSR 3.1 drop-in that replaces the game's FSR DLL.
    Fsr31Dropin,
}

impl HelixsrMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scaler => "scaler",
            Self::Fsr31Dropin => "fsr31_dropin",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Scaler => "HelixSR Scaler",
            Self::Fsr31Dropin => "HelixSR FSR 3.1 drop-in",
        }
    }
}

/// Preferred runtime wrappers the database suggests for a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrapperId {
    Gamemoderun,
    Mangohud,
    Gamescope,
}

impl WrapperId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gamemoderun => "gamemoderun",
            Self::Mangohud => "mangohud",
            Self::Gamescope => "gamescope",
        }
    }
}

/// Which DLSSNR backend a game's `[neural_rendering]` section wants.
///
/// `Auto` is resolved from the GPU at install time: RDNA 4 gets upstream, RDNA 3
/// the fork. The explicit values exist so a user override can pin one, never so
/// the database offers one architecture the other's package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NrBackendChoice {
    #[default]
    Auto,
    #[serde(alias = "rdna4-upstream")]
    Rdna4Upstream,
    #[serde(alias = "rdna3-fork")]
    Rdna3Fork,
}

impl NrBackendChoice {
    /// Resolve the choice to a backend, or `None` for `Auto`.
    pub const fn backend(self) -> Option<NrBackend> {
        match self {
            Self::Auto => None,
            Self::Rdna4Upstream => Some(NrBackend::Rdna4Upstream),
            Self::Rdna3Fork => Some(NrBackend::Rdna3Fork),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto (from the GPU)",
            Self::Rdna4Upstream => "RDNA 4 upstream",
            Self::Rdna3Fork => "RDNA 3 fork",
        }
    }
}

// ---------------------------------------------------------------------------
// Validated entry
// ---------------------------------------------------------------------------

/// One game's curated compatibility entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompatEntry {
    pub id: GameId,
    pub title: String,
    pub anticheat: AntiCheat,
    pub engine: Option<String>,
    /// Architectures this entry is offered for. Empty means unverified.
    pub verified: Vec<Arch>,
    pub upscaling: UpscalingRules,
    pub runtime: RuntimeRules,
    pub neural_rendering: Option<NeuralRules>,
}

impl CompatEntry {
    /// Parse and validate a complete entry from TOML text.
    ///
    /// The single-entry parser used by tests and by `pitagames-cli compat
    /// --validate`; the loader additionally merges user overrides first.
    pub fn from_toml(text: &str) -> Result<Self, CompatError> {
        let raw: RawEntry = toml::from_str(text).map_err(|error| CompatError::Parse {
            message: error.to_string(),
        })?;
        raw.into_entry()
    }

    /// Whether this entry is offered for `arch` without a "not tested by us" note.
    pub fn is_verified_for(&self, arch: Arch) -> bool {
        self.verified.contains(&arch)
    }

    /// The recommended upscaler for `arch`, if the entry has one.
    pub fn recommended(&self, arch: Arch) -> Option<UpscalerId> {
        self.upscaling.recommended.get(&arch).copied()
    }
}

/// The upscaling section.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UpscalingRules {
    pub supported: Vec<GameApi>,
    pub recommended: BTreeMap<Arch, UpscalerId>,
    pub preferred_dll: Option<PreferredDll>,
    pub helixsr: Option<HelixsrPlan>,
    pub notes: Option<String>,
}

/// HelixSR availability and preference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelixsrPlan {
    pub modes: Vec<HelixsrMode>,
    pub preferred: Option<HelixsrMode>,
}

/// The runtime section.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RuntimeRules {
    pub wrappers: Vec<WrapperId>,
    pub env: BTreeMap<String, String>,
    pub known_issues: Vec<String>,
}

/// The neural-rendering section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeuralRules {
    pub backend: NrBackendChoice,
    pub route: NrRoute,
    pub min_mesa: Option<Version>,
    pub dlssnr_min_version: Option<Version>,
    pub model_scale_default: BTreeMap<Arch, u8>,
    pub requires_64bit: bool,
    pub notes: Option<String>,
}

impl Default for NeuralRules {
    fn default() -> Self {
        Self {
            backend: NrBackendChoice::Auto,
            // v1 implements the OptiScaler route only; the ReShade feeders are
            // post-v1 and the profile validation refuses them by name.
            route: NrRoute::Optiscaler,
            min_mesa: None,
            dlssnr_min_version: None,
            model_scale_default: BTreeMap::new(),
            requires_64bit: true,
            notes: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

/// A parse or validation problem, labelled with the file it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompatWarning {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for CompatWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.message)
    }
}

/// The merged, validated compatibility database.
#[derive(Debug, Clone, Default)]
pub struct CompatDb {
    entries: BTreeMap<GameId, CompatEntry>,
    warnings: Vec<CompatWarning>,
    files: Vec<PathBuf>,
}

impl CompatDb {
    /// Load the database from the default shipped and user directories.
    ///
    /// Never fails: a directory that does not exist is skipped and a file that
    /// does not parse is a warning, so a broken user override cannot keep the
    /// launcher from starting.
    pub fn load() -> Self {
        Self::load_from(&default_shipped_dirs(), &default_user_dirs())
    }

    /// Load and merge explicit shipped and user directories (fixtures, a second
    /// checkout, the CLI's overrides).
    pub fn load_from(shipped: &[PathBuf], user: &[PathBuf]) -> Self {
        let mut db = Self::default();

        let mut shipped_raw = BTreeMap::new();
        let mut user_raw = BTreeMap::new();
        db.collect_layer(shipped, "shipped", &mut shipped_raw);
        db.collect_layer(user, "user", &mut user_raw);

        // User overrides merge field by field over the shipped baseline.
        let mut merged: BTreeMap<GameId, RawSlot> = shipped_raw;
        for (id, slot) in user_raw {
            match merged.get_mut(&id) {
                Some(base) => base.merge(slot),
                None => {
                    merged.insert(id, slot);
                }
            }
        }

        for (id, slot) in merged {
            let path = slot.display_sources();
            match slot.raw.into_entry() {
                Ok(entry) => {
                    db.entries.insert(id, entry);
                }
                Err(error) => db.warnings.push(CompatWarning {
                    path,
                    message: error.to_string(),
                }),
            }
        }

        db
    }

    fn collect_layer(
        &mut self,
        dirs: &[PathBuf],
        layer: &str,
        out: &mut BTreeMap<GameId, RawSlot>,
    ) {
        for dir in dirs {
            if !dir.is_dir() {
                // Missing directories are the normal case: a machine without
                // user overrides, or a shipped dir the package did not install.
                if dir.exists() {
                    self.warnings.push(CompatWarning {
                        path: dir.clone(),
                        message: "not a directory; skipping".to_string(),
                    });
                }
                continue;
            }

            let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
                Ok(entries) => entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                    .collect(),
                Err(error) => {
                    self.warnings.push(CompatWarning {
                        path: dir.clone(),
                        message: format!("could not read directory: {error}"),
                    });
                    continue;
                }
            };
            paths.sort();

            for path in paths {
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(error) => {
                        self.warnings.push(CompatWarning {
                            path: path.clone(),
                            message: format!("could not read: {error}"),
                        });
                        continue;
                    }
                };
                let raw: RawEntry = match toml::from_str(&text) {
                    Ok(raw) => raw,
                    Err(error) => {
                        self.warnings.push(CompatWarning {
                            path: path.clone(),
                            message: format!("could not parse TOML: {error}"),
                        });
                        continue;
                    }
                };

                let Some(id) = raw.identity() else {
                    self.warnings.push(CompatWarning {
                        path: path.clone(),
                        message: "missing `[game] store` and `store_id`; cannot key the entry"
                            .to_string(),
                    });
                    continue;
                };

                self.files.push(path.clone());
                match out.get_mut(&id) {
                    Some(existing) => {
                        self.warnings.push(CompatWarning {
                            path: path.clone(),
                            message: format!(
                                "duplicate {layer} entry for {}:{}; the later file wins \
                                 (already defined in {})",
                                id.store.as_str(),
                                id.store_id,
                                existing.display_sources().display()
                            ),
                        });
                        existing.merge(RawSlot {
                            raw,
                            sources: vec![path],
                        });
                    }
                    None => {
                        out.insert(
                            id,
                            RawSlot {
                                raw,
                                sources: vec![path],
                            },
                        );
                    }
                }
            }
        }
    }

    pub fn get(&self, id: &GameId) -> Option<&CompatEntry> {
        self.entries.get(id)
    }

    pub fn contains(&self, id: &GameId) -> bool {
        self.entries.contains_key(id)
    }

    /// Entries in `GameId` order (store, then store id).
    pub fn entries(&self) -> impl Iterator<Item = &CompatEntry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn warnings(&self) -> &[CompatWarning] {
        &self.warnings
    }

    /// Every file that was read successfully, in load order.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Entries that are verified for `arch`.
    pub fn verified_for(&self, arch: Arch) -> impl Iterator<Item = &CompatEntry> {
        self.entries
            .values()
            .filter(move |entry| entry.is_verified_for(arch))
    }
}

/// The default shipped directories: the environment override, the system
/// package location, then a synced copy under the data directory.
pub fn default_shipped_dirs() -> Vec<PathBuf> {
    if let Some(dir) = env_path(COMPAT_DIR_ENV) {
        return vec![dir];
    }

    let mut dirs = vec![PathBuf::from(SYSTEM_COMPAT_DIR)];
    if let Some(data) = pitagames_core::paths::data_dir() {
        dirs.push(data.join("compat"));
    }
    dirs
}

/// The default user-override directory: `$XDG_CONFIG_HOME/pitagames/compat`.
pub fn default_user_dirs() -> Vec<PathBuf> {
    pitagames_core::paths::compat_dir().into_iter().collect()
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Wire (raw) types
// ---------------------------------------------------------------------------

/// Every field is optional so a user override can be partial and is merged over
/// the shipped entry before validation. `deny_unknown_fields` turns a typo into
/// a warning instead of a silently ignored setting.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawEntry {
    game: Option<RawGame>,
    upscaling: Option<RawUpscaling>,
    runtime: Option<RawRuntime>,
    neural_rendering: Option<RawNeuralRendering>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawGame {
    store: Option<Store>,
    store_id: Option<String>,
    title: Option<String>,
    anticheat: Option<AntiCheat>,
    engine: Option<String>,
    verified: Option<Vec<Arch>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawUpscaling {
    supported: Option<Vec<GameApi>>,
    recommended: Option<BTreeMap<Arch, UpscalerId>>,
    preferred_dll: Option<PreferredDll>,
    helixsr: Option<RawHelixsr>,
    notes: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawHelixsr {
    modes: Option<Vec<HelixsrMode>>,
    preferred: Option<HelixsrMode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRuntime {
    wrappers: Option<Vec<WrapperId>>,
    env: Option<BTreeMap<String, String>>,
    known_issues: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawNeuralRendering {
    backend: Option<NrBackendChoice>,
    route: Option<NrRoute>,
    min_mesa: Option<Version>,
    dlssnr_min_version: Option<Version>,
    model_scale_default: Option<BTreeMap<Arch, u8>>,
    requires_64bit: Option<bool>,
    notes: Option<String>,
}

/// A raw entry plus the files that contributed to it, for diagnostics.
#[derive(Debug, Clone)]
struct RawSlot {
    raw: RawEntry,
    sources: Vec<PathBuf>,
}

impl RawSlot {
    fn merge(&mut self, other: RawSlot) {
        self.raw.merge(other.raw);
        self.sources.extend(other.sources);
    }

    fn display_sources(&self) -> PathBuf {
        match self.sources.as_slice() {
            [] => PathBuf::from("<unknown>"),
            [only] => only.clone(),
            many => {
                let mut joined = many[0].clone();
                for source in &many[1..] {
                    joined.push(" + ");
                    joined.push(source);
                }
                joined
            }
        }
    }
}

impl RawEntry {
    /// The game identity, if the content has one.
    fn identity(&self) -> Option<GameId> {
        let game = self.game.as_ref()?;
        let store = game.store?;
        let store_id = game.store_id.as_deref()?.trim();
        if store_id.is_empty() {
            return None;
        }
        Some(GameId::new(store, store_id))
    }

    fn merge(&mut self, other: RawEntry) {
        merge_opt(&mut self.game, other.game);
        merge_opt(&mut self.upscaling, other.upscaling);
        merge_opt(&mut self.runtime, other.runtime);
        merge_opt(&mut self.neural_rendering, other.neural_rendering);
    }

    fn into_entry(self) -> Result<CompatEntry, CompatError> {
        let game = self.game.unwrap_or_default();
        let store = game
            .store
            .ok_or_else(|| invalid("`[game] store` is required"))?;
        let store_id = game
            .store_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("`[game] store_id` is required"))?;
        let title = game
            .title
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("`[game] title` is required"))?;

        Ok(CompatEntry {
            id: GameId::new(store, store_id),
            title,
            anticheat: game.anticheat.unwrap_or_default(),
            engine: game.engine,
            verified: game.verified.unwrap_or_default(),
            upscaling: self.upscaling.unwrap_or_default().into_rules()?,
            runtime: self.runtime.unwrap_or_default().into_rules()?,
            neural_rendering: self
                .neural_rendering
                .map(RawNeuralRendering::into_rules)
                .transpose()?,
        })
    }
}

impl RawUpscaling {
    fn into_rules(self) -> Result<UpscalingRules, CompatError> {
        let helixsr = match self.helixsr {
            Some(raw) => {
                let modes = raw.modes.unwrap_or_default();
                if let Some(preferred) = raw.preferred
                    && !modes.contains(&preferred)
                {
                    return Err(invalid(format!(
                        "helixsr.preferred `{}` is not one of the listed modes",
                        preferred.as_str()
                    )));
                }
                Some(HelixsrPlan {
                    modes,
                    preferred: raw.preferred,
                })
            }
            None => None,
        };

        Ok(UpscalingRules {
            supported: self.supported.unwrap_or_default(),
            recommended: self.recommended.unwrap_or_default(),
            preferred_dll: self.preferred_dll,
            helixsr,
            notes: self.notes,
        })
    }
}

impl RawRuntime {
    fn into_rules(self) -> Result<RuntimeRules, CompatError> {
        let env = self.env.unwrap_or_default();
        for key in env.keys() {
            if !is_valid_env_key(key) {
                return Err(invalid(format!(
                    "`{key}` is not a valid environment variable name"
                )));
            }
        }
        Ok(RuntimeRules {
            wrappers: self.wrappers.unwrap_or_default(),
            env,
            known_issues: self.known_issues.unwrap_or_default(),
        })
    }
}

impl RawNeuralRendering {
    fn into_rules(self) -> Result<NeuralRules, CompatError> {
        let model_scale_default = self.model_scale_default.unwrap_or_default();
        for (arch, scale) in &model_scale_default {
            if !(MODEL_SCALE_MIN..=MODEL_SCALE_MAX).contains(scale) {
                return Err(invalid(format!(
                    "model_scale_default.{} must be within {MODEL_SCALE_MIN}..={MODEL_SCALE_MAX}, got {scale}",
                    arch.as_str()
                )));
            }
        }

        let route = self.route.unwrap_or(NrRoute::Optiscaler);
        if route != NrRoute::Optiscaler {
            return Err(invalid(format!(
                "neural_rendering.route `{}` is post-v1; only `optiscaler` ships",
                route.describe()
            )));
        }

        Ok(NeuralRules {
            backend: self.backend.unwrap_or_default(),
            route,
            min_mesa: self.min_mesa,
            dlssnr_min_version: self.dlssnr_min_version,
            model_scale_default,
            requires_64bit: self.requires_64bit.unwrap_or(true),
            notes: self.notes,
        })
    }
}

trait MergeField: Sized {
    fn merge(&mut self, other: Self);
}

impl MergeField for RawGame {
    fn merge(&mut self, other: Self) {
        set(&mut self.store, other.store);
        set(&mut self.store_id, other.store_id);
        set(&mut self.title, other.title);
        set(&mut self.anticheat, other.anticheat);
        set(&mut self.engine, other.engine);
        set(&mut self.verified, other.verified);
    }
}

impl MergeField for RawUpscaling {
    fn merge(&mut self, other: Self) {
        set(&mut self.supported, other.supported);
        set(&mut self.recommended, other.recommended);
        set(&mut self.preferred_dll, other.preferred_dll);
        set(&mut self.helixsr, other.helixsr);
        set(&mut self.notes, other.notes);
    }
}

impl MergeField for RawRuntime {
    fn merge(&mut self, other: Self) {
        set(&mut self.wrappers, other.wrappers);
        set(&mut self.env, other.env);
        set(&mut self.known_issues, other.known_issues);
    }
}

impl MergeField for RawNeuralRendering {
    fn merge(&mut self, other: Self) {
        set(&mut self.backend, other.backend);
        set(&mut self.route, other.route);
        set(&mut self.min_mesa, other.min_mesa);
        set(&mut self.dlssnr_min_version, other.dlssnr_min_version);
        set(&mut self.model_scale_default, other.model_scale_default);
        set(&mut self.requires_64bit, other.requires_64bit);
        set(&mut self.notes, other.notes);
    }
}

fn set<T>(base: &mut Option<T>, over: Option<T>) {
    if over.is_some() {
        *base = over;
    }
}

fn merge_opt<T: MergeField>(base: &mut Option<T>, over: Option<T>) {
    if let Some(over) = over {
        match base {
            Some(base) => base.merge(over),
            None => *base = Some(over),
        }
    }
}

fn is_valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

/// A problem that stops one entry from loading.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompatError {
    #[error("could not parse TOML: {message}")]
    Parse { message: String },
    #[error("invalid compat entry: {message}")]
    Invalid { message: String },
}

fn invalid(message: impl Into<String>) -> CompatError {
    CompatError::Invalid {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    /// Tests that read or write `PITAGAMES_COMPAT_DIR` must hold this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const CYBERPUNK: &str = r#"
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
anticheat = "none"
engine = "REDengine"
verified = ["rdna4"]

[upscaling]
supported = ["dlss", "fsr3", "xess"]
recommended = { rdna4 = "dlss_to_fsr4", rdna1 = "hlx_dlss", rtx = "dlss_native", arc = "dlss_to_xess" }
preferred_dll = "dbghelp.dll"
helixsr = { modes = ["scaler", "fsr31_dropin"], preferred = "scaler" }
notes = "FSR 4 needs PROTON_FSR4_UPGRADE >= 4.0.3 for MLFG"

[runtime]
wrappers = ["gamemoderun"]
known_issues = ["HDR + gamescope requires --hdr-enabled"]

[neural_rendering]
backend = "auto"
route = "optiscaler"
min_mesa = "26.2"
dlssnr_min_version = "0.0.8"
model_scale_default = { rdna4 = 100, rdna3 = 50 }
requires_64bit = true
notes = "End = NR Control, Insert = OptiScaler"
"#;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn a_full_entry_parses_and_round_trips() {
        let entry = CompatEntry::from_toml(CYBERPUNK).unwrap();
        assert_eq!(entry.id, GameId::new(Store::Steam, "1091500"));
        assert_eq!(entry.title, "Cyberpunk 2077");
        assert_eq!(entry.anticheat, AntiCheat::None);
        assert_eq!(entry.engine.as_deref(), Some("REDengine"));
        assert_eq!(entry.verified, vec![Arch::Rdna4]);
        assert!(entry.is_verified_for(Arch::Rdna4));
        assert!(!entry.is_verified_for(Arch::Rdna3));

        assert_eq!(
            entry.upscaling.supported,
            vec![GameApi::Dlss, GameApi::Fsr3, GameApi::Xess]
        );
        assert_eq!(entry.recommended(Arch::Rdna4), Some(UpscalerId::DlssToFsr4));
        assert_eq!(entry.upscaling.preferred_dll, Some(PreferredDll::Dbghelp));
        let helixsr = entry.upscaling.helixsr.as_ref().unwrap();
        assert_eq!(helixsr.preferred, Some(HelixsrMode::Scaler));
        assert_eq!(helixsr.modes.len(), 2);
        assert_eq!(entry.runtime.wrappers, vec![WrapperId::Gamemoderun]);

        let neural = entry.neural_rendering.as_ref().unwrap();
        assert_eq!(neural.backend, NrBackendChoice::Auto);
        assert_eq!(neural.route, NrRoute::Optiscaler);
        assert_eq!(
            neural.min_mesa,
            Some(Version {
                major: 26,
                minor: 2,
                patch: 0
            })
        );
        assert_eq!(
            neural.dlssnr_min_version,
            Some(Version {
                major: 0,
                minor: 0,
                patch: 8
            })
        );
        assert_eq!(neural.model_scale_default.get(&Arch::Rdna4), Some(&100));
        assert!(neural.requires_64bit);

        // The validated entry serializes to JSON for the CLI.
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: CompatEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn recommendation_maps_onto_the_profile_upscaler() {
        assert_eq!(UpscalerId::DlssToFsr4.to_upscaler(), Upscaler::DlssToFsr4);
        assert_eq!(UpscalerId::DlssNative.to_upscaler(), Upscaler::Native);
        assert_eq!(
            UpscalerId::Fsr4Upgrade.to_upscaler(),
            Upscaler::Fsr4Upgrade { version: None }
        );
        assert_eq!(
            PreferredDll::Dbghelp.to_injection_dll(),
            InjectionDll::Dbghelp
        );
        assert_eq!(
            NrBackendChoice::Rdna3Fork.backend(),
            Some(NrBackend::Rdna3Fork)
        );
        assert_eq!(NrBackendChoice::Auto.backend(), None);
    }

    #[test]
    fn minimal_game_section_is_enough() {
        let entry = CompatEntry::from_toml(
            r#"
[game]
store = "steam"
store_id = "620"
title = "Portal 2"
"#,
        )
        .unwrap();
        assert_eq!(entry.anticheat, AntiCheat::Unknown);
        assert!(entry.verified.is_empty());
        assert!(entry.upscaling.supported.is_empty());
        assert!(entry.neural_rendering.is_none());
        assert!(entry.runtime.known_issues.is_empty());
    }

    #[test]
    fn required_fields_are_reported() {
        for text in [
            r#"[game]
store_id = "1"
title = "x""#,
            r#"[game]
store = "steam"
title = "x""#,
            r#"[game]
store = "steam"
store_id = "1"
title = "   ""#,
        ] {
            assert!(matches!(
                CompatEntry::from_toml(text),
                Err(CompatError::Invalid { .. })
            ));
        }
    }

    #[test]
    fn unknown_fields_and_bad_values_are_rejected() {
        // A typo in a key name is an error, not a silent default.
        assert!(matches!(
            CompatEntry::from_toml(
                r#"
[game]
store = "steam"
store_id = "1"
title = "x"
typo = true
"#
            ),
            Err(CompatError::Parse { .. })
        ));

        // An unknown store, arch or upscaler id fails at parse time.
        for text in [
            r#"[game]
store = "origin"
store_id = "1"
title = "x""#,
            r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[upscaling]
recommended = { voodoo = "fsr4_upgrade" }
"#,
            r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[upscaling]
recommended = { rdna4 = "magic" }
"#,
        ] {
            assert!(matches!(
                CompatEntry::from_toml(text),
                Err(CompatError::Parse { .. })
            ));
        }
    }

    #[test]
    fn neural_rendering_gates_are_validated() {
        // Out-of-range model scale.
        assert!(matches!(
            CompatEntry::from_toml(
                r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[neural_rendering]
model_scale_default = { rdna4 = 10 }
"#
            ),
            Err(CompatError::Invalid { .. })
        ));

        // The post-v1 ReShade routes are refused by name.
        assert!(matches!(
            CompatEntry::from_toml(
                r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[neural_rendering]
route = "reshade_feeder"
"#
            ),
            Err(CompatError::Invalid { .. })
        ));

        // A malformed version never reaches validation.
        assert!(matches!(
            CompatEntry::from_toml(
                r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[neural_rendering]
min_mesa = "soon"
"#
            ),
            Err(CompatError::Parse { .. })
        ));
    }

    #[test]
    fn invalid_env_keys_are_rejected() {
        for key in ["", "1BAD", "WITH=EQUALS", "with space"] {
            let text = format!(
                r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[runtime]
env = {{ "{key}" = "1" }}
"#
            );
            assert!(
                matches!(
                    CompatEntry::from_toml(&text),
                    Err(CompatError::Invalid { .. })
                ),
                "accepted env key {key:?}"
            );
        }

        CompatEntry::from_toml(
            r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[runtime]
env = { DXVK_HDR = "1" }
"#,
        )
        .unwrap();
    }

    #[test]
    fn helixsr_preferred_must_be_one_of_the_modes() {
        assert!(matches!(
            CompatEntry::from_toml(
                r#"
[game]
store = "steam"
store_id = "1"
title = "x"

[upscaling]
helixsr = { modes = ["scaler"], preferred = "fsr31_dropin" }
"#
            ),
            Err(CompatError::Invalid { .. })
        ));
    }

    #[test]
    fn version_parsing_is_strict_and_ordered() {
        assert_eq!(
            Version::parse("26.2"),
            Some(Version {
                major: 26,
                minor: 2,
                patch: 0
            })
        );
        assert_eq!(
            Version::parse("26.2.3"),
            Some(Version {
                major: 26,
                minor: 2,
                patch: 3
            })
        );
        assert!(Version::parse("26").is_none());
        assert!(Version::parse("26.2.3.4").is_none());
        assert!(Version::parse("x.y").is_none());

        assert!(Version::parse("26.2").unwrap() < Version::parse("26.2.3").unwrap());
        assert!(Version::parse("25.9").unwrap() < Version::parse("26.2").unwrap());
        assert_eq!(Version::parse("26.2").unwrap().to_string(), "26.2");
        assert_eq!(Version::parse("26.2.3").unwrap().to_string(), "26.2.3");
    }

    #[test]
    fn user_overrides_merge_field_by_field_over_shipped() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = dir.path().join("shipped");
        let user = dir.path().join("user");
        std::fs::create_dir_all(&shipped).unwrap();
        std::fs::create_dir_all(&user).unwrap();

        write(
            &shipped,
            "steam-1091500.toml",
            r#"
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
engine = "REDengine"
verified = ["rdna4"]

[upscaling]
supported = ["dlss", "fsr3"]
["upscaling".recommended]
rdna4 = "dlss_to_fsr4"

[runtime]
wrappers = ["gamemoderun"]
known_issues = ["HDR + gamescope"]
"#,
        );
        // The user fixes only the recommendation and adds an issue.
        write(
            &user,
            "steam-1091500.toml",
            r#"
[game]
store = "steam"
store_id = "1091500"

[upscaling]
["upscaling".recommended]
rdna1 = "hlx_dlss"
"#,
        );

        let db = CompatDb::load_from(&[shipped], &[user]);
        assert!(db.warnings().is_empty(), "{:?}", db.warnings());
        let entry = db.get(&GameId::new(Store::Steam, "1091500")).unwrap();

        // Untouched shipped fields survive...
        assert_eq!(entry.title, "Cyberpunk 2077");
        assert_eq!(entry.engine.as_deref(), Some("REDengine"));
        assert_eq!(entry.runtime.wrappers, vec![WrapperId::Gamemoderun]);
        assert_eq!(entry.runtime.known_issues, vec!["HDR + gamescope"]);
        // ...the overridden map replaces the shipped one (merge is per field).
        assert_eq!(entry.recommended(Arch::Rdna1), Some(UpscalerId::HlxDlss));
        assert_eq!(entry.recommended(Arch::Rdna4), None);
    }

    #[test]
    fn user_can_add_a_game_the_shipped_db_does_not_know() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        write(
            &user,
            "steam-999.toml",
            r#"
[game]
store = "steam"
store_id = "999"
title = "New Game"
"#,
        );

        let db = CompatDb::load_from(&[], &[user]);
        assert_eq!(db.len(), 1);
        assert!(db.contains(&GameId::new(Store::Steam, "999")));
        assert_eq!(db.files().len(), 1);
    }

    #[test]
    fn duplicates_within_a_layer_warn_and_the_later_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = dir.path().join("shipped");
        std::fs::create_dir_all(&shipped).unwrap();

        write(
            &shipped,
            "a.toml",
            r#"
[game]
store = "steam"
store_id = "1"
title = "First"
"#,
        );
        write(
            &shipped,
            "b.toml",
            r#"
[game]
store = "steam"
store_id = "1"
title = "Second"
"#,
        );

        let db = CompatDb::load_from(&[shipped], &[]);
        assert_eq!(db.len(), 1);
        assert_eq!(db.warnings().len(), 1);
        assert!(db.warnings()[0].message.contains("duplicate shipped entry"));
        assert_eq!(
            db.get(&GameId::new(Store::Steam, "1")).unwrap().title,
            "Second"
        );
    }

    #[test]
    fn broken_files_become_warnings_not_failures() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = dir.path().join("shipped");
        std::fs::create_dir_all(&shipped).unwrap();

        write(&shipped, "bad-toml.toml", "this is not = toml [");
        write(&shipped, "no-game.toml", "[upscaling]\nsupported = []\n");
        write(
            &shipped,
            "missing-field.toml",
            r#"
[game]
store = "steam"
store_id = "2"
"#,
        );
        write(&shipped, "ignored.txt", "not a toml file");

        let db = CompatDb::load_from(&[shipped], &[]);
        assert!(db.is_empty());
        assert_eq!(db.warnings().len(), 3, "{:?}", db.warnings());
        assert!(
            db.warnings()
                .iter()
                .any(|w| w.message.contains("parse TOML"))
        );
        assert!(
            db.warnings()
                .iter()
                .any(|w| w.message.contains("cannot key the entry"))
        );
        assert!(db.warnings().iter().any(|w| w.message.contains("title")));
    }

    #[test]
    fn missing_directories_are_silent_and_a_file_in_their_place_warns() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let db = CompatDb::load_from(std::slice::from_ref(&missing), &[]);
        assert!(db.is_empty());
        assert!(db.warnings().is_empty());

        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let db = CompatDb::load_from(&[file], &[]);
        assert!(
            db.warnings()
                .iter()
                .any(|w| w.message.contains("not a directory"))
        );
    }

    #[test]
    fn the_repository_database_is_warning_free() {
        // The real shipped entries must always load cleanly; this is what keeps
        // a bad curation commit out of `compat/`.
        let shipped = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("compat");
        let db = CompatDb::load_from(&[shipped], &[]);
        assert!(
            db.warnings().is_empty(),
            "shipped compat warnings: {:?}",
            db.warnings()
        );
        assert!(
            db.len() >= 30,
            "expected at least 30 curated entries, got {}",
            db.len()
        );

        // The milestone's "RDNA 4 verified" target: the curated set ships as
        // verified for our primary card, and no entry is keyed twice (a
        // duplicate would already have produced a warning).
        assert_eq!(db.verified_for(Arch::Rdna4).count(), db.len());
    }

    #[test]
    fn default_dirs_honor_the_environment_override() {
        let guard = ENV_LOCK.lock().unwrap();
        // SAFETY: only this test mutates PITAGAMES_COMPAT_DIR.
        unsafe { std::env::set_var(COMPAT_DIR_ENV, "/tmp/pitagames-compat") };
        let dirs = default_shipped_dirs();
        unsafe { std::env::remove_var(COMPAT_DIR_ENV) };
        drop(guard);

        assert_eq!(dirs, vec![PathBuf::from("/tmp/pitagames-compat")]);
    }

    #[test]
    fn default_shipped_dirs_include_the_system_location() {
        let guard = ENV_LOCK.lock().unwrap();
        let dirs = default_shipped_dirs();
        drop(guard);

        assert_eq!(dirs.first(), Some(&PathBuf::from(SYSTEM_COMPAT_DIR)));
    }
}
