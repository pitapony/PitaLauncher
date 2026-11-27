//! The neural-rendering backend registry, gates and cost data.
//!
//! DLSSNR-AMD is two independent codebases with one installer UX, and the
//! launcher must never blur them: the **RDNA 4** upstream lives on GitLab
//! (`mochizuki0323/DLSSNR-AMD`) and is the path we validate; the **RDNA 3**
//! Linux fork lives on GitHub (`mauri870/DLSSNR-RDNA3`) and ships
//! `experimental`. Each is pinned separately in shipped data and a card is
//! never offered the other's package.
//!
//! Three things live here, all framework-free:
//!
//! - [`BackendRegistry`] — the shipped + user pin file. One [`BackendPin`] per
//!   backend resolves to the [`PinnedRelease`] `mods::release` fetches and
//!   SHA-256-verifies, so a digest that is not the pin is refused (ADR-0024).
//! - [`plan`] — the backend verdict. It delegates the coarse gates (arch, the
//!   single injection slot, game curation, the package's 64-bit flag, Mesa and
//!   Proton) to [`crate::capability`] so there is one gate order, then adds the
//!   backend-specific gates: the **cross-offer** rule, the game's own bitness
//!   and the **fork lag** (`dlssnr_min_version`).
//! - the **cost table** and VRAM-aware [`model_scale_preset`]s, as data: the
//!   plan §6 numbers drive the UI's default `model_scale` per resolution and
//!   GPU, so an 8 GB RDNA 3 card is not handed the 4K full-network default.
//!
//! ```text
//!  capability::availability(NeuralRendering) ──► Hidden(reason)?  ──► stop
//!          │ Available
//!          ▼
//!  backend = profile ▸ game curation ▸ card     cross-offer? ──────► Hidden
//!          │
//!          ▼
//!  registry.get(backend) ──► None ─────────────────────────────────► Hidden
//!          │ Some(pin)
//!          ▼
//!  pin.version < dlssnr_min_version? ──────────────────────────────► Hidden
//!          │
//!          ▼
//!  NrPlan { backend, pin, experimental }
//! ```
//!
//! The live HTTP [`Fetcher`](pitagames_mods::release::Fetcher) is still ticket
//! #12's open item, so the tests and `pitagames-cli nr` drive the whole
//! fetch → verify pipeline from a fixture with no network.

use std::fs;
use std::path::{Path, PathBuf};

use pitagames_core::profile::{NeuralRendering, NrBackend, Profile};
use pitagames_mods::release::{
    FetchedRelease, Fetcher, PinnedRelease, ReleaseError, ReleaseSource,
};
use serde::{Deserialize, Serialize};

use crate::capability::{Availability, Field};
use crate::compat::{Arch, CompatEntry, Version};
use crate::gpu::{GpuArch, GpuProfile};
use crate::proton::ProtonInstall;

/// The shipped backend pin file, embedded at build time.
///
/// It is data, not code: the pins are the "shipped data" ADR-0024 requires for
/// every release fetch, so an upstream tag moving cannot silently become a
/// different install. A user file (see [`default_user_path`]) can add or replace
/// a pin.
pub const SHIPPED: &str = include_str!("../data/dlssnr-backends.toml");

/// Mesa version DLSSNR (neural rendering) requires.
///
/// The same floor [`crate::capability`] enforces; kept here so the backend gate
/// and the matrix cannot drift.
pub const MESA_MIN: Version = Version {
    major: 26,
    minor: 2,
    patch: 0,
};

// ---------------------------------------------------------------------------
// The reason strings — the single source for the UI and the docs table
// ---------------------------------------------------------------------------

/// Every "why is this hidden" string the backend plan can produce.
///
/// The coarse gates reuse [`crate::capability::reason`] so the UI copy cannot
/// drift between the matrix and the backend plan. A test asserts every string
/// below appears verbatim in `docs/dlssnr.md`.
pub mod reason {
    pub use crate::capability::reason::{
        GAME_NO_NR, GAME_NOT_CURATED, NR_32BIT, NR_MESA_UNKNOWN, NR_NEEDS_MESA, NR_NEEDS_RDNA3_4,
    };

    /// A card was offered the RDNA 4 package but is not RDNA 4.
    pub const RDNA4_ONLY: &str = "the RDNA 4 upstream package is offered only on RDNA 4";
    /// A card was offered the RDNA 3 fork but is not RDNA 3.
    pub const RDNA3_ONLY: &str = "the RDNA 3 fork is offered only on RDNA 3";
    /// The resolved backend has no shipped/caller pin.
    pub const BACKEND_NOT_PINNED: &str =
        "no release is pinned for this neural-rendering backend yet";
    /// The pinned release is older than the game's curated minimum.
    pub const PIN_BEHIND: &str = "the pinned DLSSNR release is older than this game requires";
}

/// Every [`reason`] string, for the docs-agreement test and the CLI's `--help`.
pub const REASONS: &[&str] = &[
    reason::NR_NEEDS_RDNA3_4,
    reason::NR_NEEDS_MESA,
    reason::NR_MESA_UNKNOWN,
    reason::NR_32BIT,
    reason::GAME_NO_NR,
    reason::GAME_NOT_CURATED,
    reason::RDNA4_ONLY,
    reason::RDNA3_ONLY,
    reason::BACKEND_NOT_PINNED,
    reason::PIN_BEHIND,
];

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

/// The backend an architecture is allowed to use, or `None` when the card has
/// no neural-rendering path (RDNA 1/2, NVIDIA, Intel, unknown).
///
/// This is the cross-offer rule in one place: RDNA 4 maps to the upstream
/// package, RDNA 3 to the fork, and nothing else maps anywhere.
pub const fn backend_for(arch: GpuArch) -> Option<NrBackend> {
    match arch {
        GpuArch::Rdna4 => Some(NrBackend::Rdna4Upstream),
        GpuArch::Rdna3 => Some(NrBackend::Rdna3Fork),
        _ => None,
    }
}

/// Whether a backend is the unvalidated fork.
///
/// The RDNA 3 fork is a Linux-only port tested only on one card, so the UI must
/// mark it "not tested by us" (ADR-0004).
pub const fn is_experimental(backend: NrBackend) -> bool {
    matches!(backend, NrBackend::Rdna3Fork)
}

/// The short label for a backend, matching the pin file's `backend` value.
pub const fn backend_label(backend: NrBackend) -> &'static str {
    match backend {
        NrBackend::Rdna4Upstream => "rdna4_upstream",
        NrBackend::Rdna3Fork => "rdna3_fork",
    }
}

/// The reason a card cannot be offered the backend it asked for.
const fn cross_offer_reason(want: NrBackend) -> &'static str {
    match want {
        NrBackend::Rdna4Upstream => reason::RDNA4_ONLY,
        NrBackend::Rdna3Fork => reason::RDNA3_ONLY,
    }
}

/// Resolve a requested backend against the card, refusing a cross-offer.
///
/// `requested` is the profile's or the game's explicit choice (`None` for
/// "auto"); a choice that contradicts the card is refused rather than silently
/// swapped, so an RDNA 3 card is never handed the RDNA 4 package and vice
/// versa.
pub fn resolve(requested: Option<NrBackend>, arch: GpuArch) -> Result<NrBackend, &'static str> {
    let allowed = backend_for(arch).ok_or(reason::NR_NEEDS_RDNA3_4)?;
    match requested {
        Some(want) if want != allowed => Err(cross_offer_reason(want)),
        _ => Ok(allowed),
    }
}

// ---------------------------------------------------------------------------
// Pins and the registry
// ---------------------------------------------------------------------------

/// One pinned backend release.
///
/// `version` is carried explicitly (rather than parsed from `tag`) because the
/// two hosts tag differently; it is what the fork-lag gate compares against the
/// game's `dlssnr_min_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendPin {
    /// Which architecture family this pin belongs to.
    pub backend: NrBackend,
    /// The release host (GitLab upstream, GitHub fork).
    pub source: ReleaseSource,
    /// `owner/repo`.
    pub project: String,
    /// The pinned tag.
    pub tag: String,
    /// The release version, for the fork-lag gate.
    pub version: Version,
    /// The exact asset file name to fetch.
    pub asset: String,
    /// The pinned SHA-256 of that asset, lowercase hex.
    pub sha256: String,
}

impl BackendPin {
    /// The [`PinnedRelease`] this pin resolves to.
    pub fn pinned(&self) -> PinnedRelease {
        PinnedRelease {
            source: self.source,
            project: self.project.clone(),
            tag: self.tag.clone(),
            asset: self.asset.clone(),
            sha256: self.sha256.clone(),
        }
    }

    /// Whether this backend is the unvalidated fork.
    pub const fn is_experimental(&self) -> bool {
        is_experimental(self.backend)
    }

    /// Fetch, SHA-256-verify and unpack this pin into `dest`.
    ///
    /// The convenience the CLI and the installer both use; a digest that is not
    /// the pin is refused by [`PinnedRelease::fetch_and_unpack`].
    pub fn fetch_and_unpack(
        &self,
        fetcher: &dyn Fetcher,
        dest: &Path,
    ) -> Result<FetchedRelease, ReleaseError> {
        self.pinned().fetch_and_unpack(fetcher, dest)
    }
}

/// A set of pinned backends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendRegistry {
    /// The pins, in file order.
    #[serde(default)]
    pub backends: Vec<BackendPin>,
}

impl BackendRegistry {
    /// Parse a registry document.
    pub fn parse(text: &str) -> Result<Self, RegistryError> {
        let registry: Self = toml::from_str(text)?;
        registry.validate()?;
        Ok(registry)
    }

    /// Read a registry from a file.
    pub fn load(path: &Path) -> Result<Self, RegistryError> {
        let text = fs::read_to_string(path).map_err(|source| RegistryError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }

    /// The shipped registry.
    ///
    /// The embedded file is compiled data, so a malformed one is a build bug; a
    /// test asserts it parses.
    pub fn shipped() -> Self {
        Self::parse(SHIPPED).expect("the shipped DLSSNR backend registry parses")
    }

    /// The shipped registry with a user file merged over it.
    ///
    /// A user pin for a backend replaces the shipped one (so a user can move to
    /// a newer release without waiting for a PitaGames update); a new backend is
    /// added. A missing user file is not an error.
    pub fn effective(user_path: Option<&Path>) -> Result<Self, RegistryError> {
        let mut registry = Self::shipped();
        if let Some(path) = user_path
            && path.is_file()
        {
            registry = registry.merge(Self::load(path)?);
        }
        Ok(registry)
    }

    /// Fold `other` in, `other`'s pin winning on a backend collision.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        for pin in other.backends {
            match self
                .backends
                .iter_mut()
                .find(|existing| existing.backend == pin.backend)
            {
                Some(existing) => *existing = pin,
                None => self.backends.push(pin),
            }
        }
        self
    }

    /// The pin for this backend, if any.
    pub fn get(&self, backend: NrBackend) -> Option<&BackendPin> {
        self.backends.iter().find(|pin| pin.backend == backend)
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// The pins, in file order.
    pub fn all(&self) -> &[BackendPin] {
        &self.backends
    }

    /// Refuse a document with a bad digest or a duplicated backend.
    fn validate(&self) -> Result<(), RegistryError> {
        for pin in &self.backends {
            if pin.sha256.len() != 64 || !pin.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(RegistryError::InvalidPin {
                    backend: backend_label(pin.backend),
                    detail: format!("`{}` is not a 64-hex SHA-256", pin.sha256),
                });
            }
        }
        Ok(())
    }
}

/// `<XDG_CONFIG_HOME>/pitagames/dlssnr/backends.toml`: the user pin override.
pub fn default_user_path() -> Option<PathBuf> {
    pitagames_core::paths::config_dir().map(|dir| dir.join("dlssnr").join("backends.toml"))
}

/// Why a registry could not be read.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the DLSSNR backend registry could not be read: {source}")]
    Toml {
        #[from]
        source: toml::de::Error,
    },

    #[error("the `{backend}` pin is invalid: {detail}")]
    InvalidPin {
        backend: &'static str,
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// The resolved backend and the pinned release behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NrPlan {
    /// The backend the card is allowed to use.
    pub backend: NrBackend,
    /// Its pinned release.
    pub pin: BackendPin,
    /// Whether it is the unvalidated RDNA 3 fork.
    pub experimental: bool,
}

/// Whether the launcher may offer neural rendering, and with which backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NrVerdict {
    /// Neural rendering is offered through this backend.
    Available(NrPlan),
    /// It is hidden, with the one-line reason the UI shows.
    Hidden { reason: String },
}

impl NrVerdict {
    /// The plan, when available.
    pub fn plan(&self) -> Option<&NrPlan> {
        match self {
            Self::Available(plan) => Some(plan),
            Self::Hidden { .. } => None,
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }

    /// The reason, when hidden.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available(_) => None,
            Self::Hidden { reason } => Some(reason),
        }
    }
}

/// Everything [`plan`] needs.
///
/// Built with the `with_*` setters so a caller supplies only what it has: the
/// CLI without a game passes no compat entry, the GUI passes the resolved
/// Proton and the game's own bitness.
#[derive(Debug, Clone)]
pub struct NrRequest<'a> {
    pub gpu: &'a GpuProfile,
    pub profile: &'a Profile,
    pub registry: &'a BackendRegistry,
    pub compat: Option<&'a CompatEntry>,
    pub proton: Option<&'a ProtonInstall>,
    /// The game's own bitness, from [`pe_is_64_bit`]; `None` when unknown.
    pub game_64bit: Option<bool>,
}

impl<'a> NrRequest<'a> {
    /// A request with the machine, the profile and the registry.
    pub fn new(gpu: &'a GpuProfile, profile: &'a Profile, registry: &'a BackendRegistry) -> Self {
        Self {
            gpu,
            profile,
            registry,
            compat: None,
            proton: None,
            game_64bit: None,
        }
    }

    #[must_use]
    pub fn with_compat(mut self, compat: Option<&'a CompatEntry>) -> Self {
        self.compat = compat;
        self
    }

    #[must_use]
    pub fn with_proton(mut self, proton: Option<&'a ProtonInstall>) -> Self {
        self.proton = proton;
        self
    }

    #[must_use]
    pub fn with_game_64bit(mut self, game_64bit: Option<bool>) -> Self {
        self.game_64bit = game_64bit;
        self
    }
}

/// Decide which neural-rendering backend this machine and game get, if any.
///
/// The coarse gates — the card, the single injection slot, game curation, the
/// package's 64-bit flag, Mesa and Proton — are [`crate::capability`]'s, so the
/// matrix and this plan can never disagree. On top of those:
///
/// 1. **Cross-offer.** The requested backend (the profile's choice, else the
///    game's curated choice, else the card's) must match the card.
/// 2. **The game's own bitness.** A 32-bit game is refused (`NR_32BIT`) even
///    when the package's `pkg_arch` is x86_64.
/// 3. **The pin.** The backend must have a pinned release, and its version must
///    not be older than the game's `dlssnr_min_version` (the fork-lag gate).
pub fn plan(request: NrRequest<'_>) -> NrVerdict {
    // 1. The coarse matrix gate: arch, slot, curation, pkg_arch, Mesa, Proton.
    if let Availability::Hidden { reason } = crate::capability::availability(
        Field::NeuralRendering,
        request.profile,
        request.gpu,
        request.compat,
        request.proton,
    ) {
        return NrVerdict::Hidden { reason };
    }

    // 2. Resolve the backend, refusing a cross-offer.
    let backend = match resolve(
        requested_backend(request.profile, request.compat),
        request.gpu.arch,
    ) {
        Ok(backend) => backend,
        Err(reason) => {
            return NrVerdict::Hidden {
                reason: reason.to_string(),
            };
        }
    };

    // 3. The game's own bitness (the coarse gate only saw the package arch).
    let rules = request
        .compat
        .and_then(|entry| entry.neural_rendering.as_ref());
    if rules.is_some_and(|rules| rules.requires_64bit) && request.game_64bit == Some(false) {
        return NrVerdict::Hidden {
            reason: reason::NR_32BIT.to_string(),
        };
    }

    // 4. The pin, and the fork-lag gate.
    let Some(pin) = request.registry.get(backend) else {
        return NrVerdict::Hidden {
            reason: reason::BACKEND_NOT_PINNED.to_string(),
        };
    };
    if let Some(min) = rules.and_then(|rules| rules.dlssnr_min_version)
        && pin.version < min
    {
        return NrVerdict::Hidden {
            reason: reason::PIN_BEHIND.to_string(),
        };
    }

    NrVerdict::Available(NrPlan {
        backend,
        experimental: is_experimental(backend),
        pin: pin.clone(),
    })
}

/// The backend a request asks for, before the card has its say.
///
/// The profile's own DLSSNR choice wins; with neural rendering off, the game's
/// curated choice applies; `None` means "auto, from the card".
fn requested_backend(profile: &Profile, compat: Option<&CompatEntry>) -> Option<NrBackend> {
    match &profile.neural_rendering {
        NeuralRendering::DlssNrAmd { backend, .. } => Some(*backend),
        NeuralRendering::Off => compat
            .and_then(|entry| entry.neural_rendering.as_ref())
            .and_then(|rules| rules.backend.backend()),
    }
}

// ---------------------------------------------------------------------------
// The cost table and the VRAM-aware presets
// ---------------------------------------------------------------------------

/// A display resolution the cost table is keyed by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Resolution {
    #[default]
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "4k")]
    Uhd2160,
}

impl Resolution {
    /// Every resolution, in the order the UI presents them.
    pub const ALL: [Resolution; 3] = [Self::P1080, Self::P1440, Self::Uhd2160];

    /// The stable identifier used in JSON and the CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P1080 => "1080p",
            Self::P1440 => "1440p",
            Self::Uhd2160 => "4k",
        }
    }

    /// The human label the UI shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::P1080 => "1080p",
            Self::P1440 => "1440p",
            Self::Uhd2160 => "4K",
        }
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A cost estimate: the network's own frame time and its VRAM need.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// The estimated network cost in milliseconds.
    pub millis: f32,
    /// The estimated VRAM the network needs, in bytes.
    pub vram_bytes: u64,
}

/// The measured 100 % cost, per backend and resolution (plan §6 table).
///
/// RDNA 4's figures are the RX 9070 XT `CC_SMALL_FP8` numbers; the fork's are
/// the RX 7900 XTX defaults, which cost ~2.6× because FP8 is emulated in FP16.
const RDNA4_100: [Cost; 3] = [
    Cost {
        millis: 3.3,
        vram_bytes: 650_000_000,
    },
    Cost {
        millis: 5.5,
        vram_bytes: 1_160_000_000,
    },
    Cost {
        millis: 12.7,
        vram_bytes: 2_600_000_000,
    },
];
const RDNA3_100: [Cost; 3] = [
    Cost {
        millis: 15.0,
        vram_bytes: 1_330_000_000,
    },
    Cost {
        millis: 26.0,
        vram_bytes: 2_360_000_000,
    },
    Cost {
        millis: 56.0,
        vram_bytes: 5_300_000_000,
    },
];

const fn base_index(resolution: Resolution) -> usize {
    match resolution {
        Resolution::P1080 => 0,
        Resolution::P1440 => 1,
        Resolution::Uhd2160 => 2,
    }
}

/// The 100 % cost for a backend at a resolution.
fn base_cost(backend: NrBackend, resolution: Resolution) -> Cost {
    let table = match backend {
        NrBackend::Rdna4Upstream => &RDNA4_100,
        NrBackend::Rdna3Fork => &RDNA3_100,
    };
    table[base_index(resolution)]
}

/// The estimated cost at a `model_scale` percent.
///
/// `model_scale` is a linear-resolution factor, so the cost grows with its
/// square: half the network resolution is roughly a quarter of the cost. The
/// figure is an estimate, not a measurement.
pub fn cost(backend: NrBackend, resolution: Resolution, model_scale: u8) -> Cost {
    let base = base_cost(backend, resolution);
    let factor = (f32::from(model_scale) / 100.0).powi(2);
    Cost {
        millis: (base.millis * factor * 10.0).round() / 10.0,
        vram_bytes: (base.vram_bytes as f64 * f64::from(factor)).round() as u64,
    }
}

/// The default `model_scale` percent for a backend, resolution and VRAM size.
///
/// The resolution drives the base: RDNA 4 runs the full network everywhere
/// (≈2.6 GB at 4K); the RDNA 3 fork halves it at 4K, where the full network
/// costs ≈5.3 GB and 56 ms. The card's VRAM then caps it — the scale steps down
/// a quarter at a time until the network's estimated VRAM fits within **half**
/// the card, leaving the other half for the game. An unknown VRAM size applies
/// no cap (never guess).
pub fn model_scale_preset(
    backend: NrBackend,
    resolution: Resolution,
    vram_bytes: Option<u64>,
) -> u8 {
    let mut scale = base_scale(backend, resolution);
    let Some(vram) = vram_bytes else {
        return scale;
    };
    let budget = vram / 2;
    while scale > NeuralRendering::MODEL_SCALE_MIN
        && cost(backend, resolution, scale).vram_bytes > budget
    {
        scale -= 25;
    }
    scale
}

/// The resolution-only default, before the VRAM cap.
fn base_scale(backend: NrBackend, resolution: Resolution) -> u8 {
    match backend {
        NrBackend::Rdna4Upstream => 100,
        // 4K on the fork is unplayable at full size (56 ms), so it defaults to
        // half the network resolution (plan §6).
        NrBackend::Rdna3Fork => match resolution {
            Resolution::Uhd2160 => 50,
            _ => 100,
        },
    }
}

/// The game's curated `model_scale` for this card, if its entry pins one.
pub fn curated_scale(compat: Option<&CompatEntry>, arch: GpuArch) -> Option<u8> {
    let arch: Arch = arch.compat_arch()?;
    compat?
        .neural_rendering
        .as_ref()?
        .model_scale_default
        .get(&arch)
        .copied()
}

/// The UI's default `model_scale`: the game's curated value when it has one,
/// else the VRAM-aware preset.
pub fn default_scale(
    compat: Option<&CompatEntry>,
    backend: NrBackend,
    arch: GpuArch,
    resolution: Resolution,
    vram_bytes: Option<u64>,
) -> u8 {
    curated_scale(compat, arch)
        .unwrap_or_else(|| model_scale_preset(backend, resolution, vram_bytes))
}

// ---------------------------------------------------------------------------
// The game's own bitness (the PE header check capability defers to the backends)
// ---------------------------------------------------------------------------

/// Whether a Windows game executable is 64-bit, read from its PE header.
///
/// DLSSNR ships x86_64 only in v1, so a 32-bit game is refused rather than
/// offered a package that cannot load. `goblin` reads the header; a file that
/// is not a PE is an error, because a non-Windows game has no NR path anyway.
pub fn pe_is_64_bit(exe: &Path) -> Result<bool, NrError> {
    let bytes = fs::read(exe).map_err(|source| NrError::Io {
        path: exe.to_path_buf(),
        source,
    })?;
    let pe = goblin::pe::PE::parse(&bytes).map_err(|error| NrError::Binary {
        path: exe.to_path_buf(),
        detail: error.to_string(),
    })?;
    Ok(pe.is_64)
}

/// Why a backend or a game binary could not be read.
#[derive(Debug, thiserror::Error)]
pub enum NrError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a Windows executable: {detail}")]
    Binary { path: PathBuf, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::NrBackendChoice;
    use crate::gpu::HdrSupport;
    use pitagames_core::profile::{NrModel, NrProcessing, NrRoute, PkgArch};
    use pitagames_core::{GameId, Store};
    use pitagames_mods::release::{MapFetcher, sha256_hex};

    const CYBERPUNK: &str = r#"
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
anticheat = "none"

[upscaling]
supported = ["dlss"]

[neural_rendering]
backend = "auto"
route = "optiscaler"
min_mesa = "26.2"
dlssnr_min_version = "0.0.8"
model_scale_default = { rdna4 = 100, rdna3 = 50 }
requires_64bit = true
"#;

    fn game() -> GameId {
        GameId::new(Store::Steam, "1091500")
    }

    fn profile() -> Profile {
        Profile::new(game(), "nr")
    }

    fn nr_profile(backend: NrBackend) -> Profile {
        let mut profile = profile();
        profile.neural_rendering = NeuralRendering::DlssNrAmd {
            backend,
            version: "0.0.8".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 100,
            processing: NrProcessing::RenoDx,
            pkg_arch: PkgArch::X86_64,
            captured_env: Default::default(),
            model_source: pitagames_core::profile::ModelSource::CachedInPackage,
        };
        profile
    }

    fn gpu(arch: GpuArch, vram_gb: Option<u64>) -> GpuProfile {
        GpuProfile {
            vendor: arch.vendor(),
            arch,
            device_id: None,
            name: None,
            mesa: Some(MESA_MIN),
            vram_bytes: vram_gb.map(|gb| gb * 1_000_000_000),
            ntsync: false,
            hdr: HdrSupport::Unknown,
            warnings: Vec::new(),
        }
    }

    fn cyberpunk() -> CompatEntry {
        CompatEntry::from_toml(CYBERPUNK).unwrap()
    }

    fn registry() -> BackendRegistry {
        BackendRegistry::shipped()
    }

    /// A recent GE build, so the capability matrix's Proton gate passes.
    fn recent_proton() -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/steam/compatibilitytools.d/GE-Proton"),
            name: "GE-Proton".to_string(),
            kind: crate::proton::ProtonKind::Ge,
            version: Some("GE-Proton10-1".to_string()),
            source: crate::proton::ProtonSource::UserCompatibilityTools,
        }
    }

    /// A VRAM size in bytes, for the preset tests.
    fn gb(count: u64) -> Option<u64> {
        Some(count * 1_000_000_000)
    }

    // -- The shipped registry -------------------------------------------------

    #[test]
    fn the_shipped_registry_pins_both_backends() {
        let registry = BackendRegistry::shipped();
        let upstream = registry
            .get(NrBackend::Rdna4Upstream)
            .expect("upstream pin");
        assert_eq!(upstream.source, ReleaseSource::GitLab);
        assert_eq!(upstream.project, "mochizuki0323/DLSSNR-AMD");
        assert!(!upstream.is_experimental());
        assert_eq!(upstream.sha256.len(), 64);

        let fork = registry.get(NrBackend::Rdna3Fork).expect("fork pin");
        assert_eq!(fork.source, ReleaseSource::GitHub);
        assert_eq!(fork.project, "mauri870/DLSSNR-RDNA3");
        assert!(fork.is_experimental(), "the fork is the experimental path");
    }

    #[test]
    fn a_user_pin_replaces_a_shipped_one_by_backend() {
        let shipped = BackendRegistry::shipped();
        let replacement = BackendRegistry {
            backends: vec![BackendPin {
                backend: NrBackend::Rdna3Fork,
                sha256: "a".repeat(64),
                ..shipped.get(NrBackend::Rdna3Fork).unwrap().clone()
            }],
        };

        let merged = BackendRegistry::shipped().merge(replacement);
        assert_eq!(
            merged.get(NrBackend::Rdna3Fork).unwrap().sha256,
            "a".repeat(64)
        );
        assert_eq!(
            merged.backends.len(),
            shipped.backends.len(),
            "no duplicate backend"
        );
    }

    #[test]
    fn a_bad_digest_is_refused_at_load() {
        let text = r#"
[[backends]]
backend = "rdna4_upstream"
source = "gitlab"
project = "mochizuki0323/DLSSNR-AMD"
tag = "v0.0.8"
version = "0.0.8"
asset = "DLSSNR-AMD.tar.gz"
sha256 = "not-a-digest"
"#;
        let error = BackendRegistry::parse(text).unwrap_err();
        assert!(matches!(error, RegistryError::InvalidPin { .. }), "{error}");
    }

    #[test]
    fn a_missing_user_registry_is_not_an_error() {
        let registry =
            BackendRegistry::effective(Some(Path::new("/nowhere/backends.toml"))).unwrap();
        assert_eq!(registry, BackendRegistry::shipped());
    }

    #[test]
    fn a_user_registry_file_overrides_the_shipped_pins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backends.toml");
        fs::write(
            &path,
            r#"
[[backends]]
backend = "rdna4_upstream"
source = "gitlab"
project = "mochizuki0323/DLSSNR-AMD"
tag = "v0.0.9"
version = "0.0.9"
asset = "DLSSNR-AMD-Vulkan-Linux-0.0.9-x86_64.tar.gz"
sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#,
        )
        .unwrap();

        let registry = BackendRegistry::effective(Some(&path)).unwrap();
        assert_eq!(
            registry
                .get(NrBackend::Rdna4Upstream)
                .unwrap()
                .version
                .patch,
            9
        );
        // The fork pin is still there beside it.
        assert!(registry.get(NrBackend::Rdna3Fork).is_some());
    }

    // -- Pinning and verification --------------------------------------------

    fn tar_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, *name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// A fixture archive plus the pin and fetcher that resolve it offline.
    fn fixture(backend: NrBackend) -> (BackendPin, MapFetcher) {
        let archive = tar_bytes(&[("dlssnr-amd.asi", b"the plugin")]);
        let digest = sha256_hex(&archive);
        let (source, project, tag, asset) = match backend {
            NrBackend::Rdna4Upstream => (
                ReleaseSource::GitLab,
                "mochizuki0323/DLSSNR-AMD",
                "v0.0.8",
                "DLSSNR-AMD-Vulkan-Linux-0.0.8-x86_64.tar",
            ),
            NrBackend::Rdna3Fork => (
                ReleaseSource::GitHub,
                "mauri870/DLSSNR-RDNA3",
                "v0.0.9",
                "DLSSNR-AMD-Vulkan-Linux-0.0.9-x86_64.tar",
            ),
        };
        let pin = BackendPin {
            backend,
            source,
            project: project.to_string(),
            tag: tag.to_string(),
            version: Version::parse(tag.trim_start_matches('v')).unwrap(),
            asset: asset.to_string(),
            sha256: digest,
        };
        let url = format!("https://dl.test/{asset}");
        let body = match source {
            ReleaseSource::GitHub => serde_json::json!({
                "tag_name": tag,
                "assets": [{ "name": asset, "browser_download_url": url }],
            })
            .to_string(),
            ReleaseSource::GitLab => serde_json::json!({
                "tag_name": tag,
                "assets": { "links": [{ "name": asset, "direct_asset_url": url }] },
            })
            .to_string(),
        };
        let fetcher = MapFetcher::new()
            .with(source.release_url(project, tag), body)
            .with(url, archive);
        (pin, fetcher)
    }

    #[test]
    fn each_backend_resolves_its_pinned_asset_and_verifies_the_pin() {
        for backend in [NrBackend::Rdna4Upstream, NrBackend::Rdna3Fork] {
            let (pin, fetcher) = fixture(backend);
            let dir = tempfile::tempdir().unwrap();
            let report = pin.fetch_and_unpack(&fetcher, dir.path()).unwrap();
            assert_eq!(report.sha256, pin.sha256, "{backend:?}");
            assert_eq!(report.files, ["dlssnr-amd.asi"], "{backend:?}");
            assert_eq!(
                fs::read(dir.path().join("dlssnr-amd.asi")).unwrap(),
                b"the plugin"
            );
        }
    }

    #[test]
    fn a_pin_that_does_not_match_is_refused() {
        let (mut pin, fetcher) = fixture(NrBackend::Rdna4Upstream);
        pin.sha256 = "0".repeat(64);
        let dir = tempfile::tempdir().unwrap();
        let error = pin.fetch_and_unpack(&fetcher, dir.path()).unwrap_err();
        assert!(
            matches!(error, ReleaseError::DigestMismatch { .. }),
            "{error}"
        );
    }

    // -- Cross-offer ----------------------------------------------------------

    #[test]
    fn a_card_maps_to_its_own_backend() {
        assert_eq!(backend_for(GpuArch::Rdna4), Some(NrBackend::Rdna4Upstream));
        assert_eq!(backend_for(GpuArch::Rdna3), Some(NrBackend::Rdna3Fork));
        for arch in [
            GpuArch::Rdna2,
            GpuArch::Rdna1,
            GpuArch::Bmg,
            GpuArch::Rtx40,
            GpuArch::Unknown,
        ] {
            assert_eq!(backend_for(arch), None, "{arch:?} has no NR path");
        }
    }

    #[test]
    fn an_rdna3_card_is_never_offered_the_rdna4_package() {
        // The fork is what RDNA 3 gets; the upstream package is refused by name.
        assert_eq!(resolve(None, GpuArch::Rdna3), Ok(NrBackend::Rdna3Fork));
        assert_eq!(
            resolve(Some(NrBackend::Rdna4Upstream), GpuArch::Rdna3),
            Err(reason::RDNA4_ONLY)
        );
        // And the plan agrees, when the profile explicitly pins the wrong one.
        let profile = nr_profile(NrBackend::Rdna4Upstream);
        let proton = recent_proton();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna3, Some(16)), &profile, &registry())
                .with_compat(Some(&cyberpunk()))
                .with_proton(Some(&proton)),
        );
        assert_eq!(verdict.reason(), Some(reason::RDNA4_ONLY));
    }

    #[test]
    fn an_rdna4_card_is_never_offered_the_fork() {
        assert_eq!(resolve(None, GpuArch::Rdna4), Ok(NrBackend::Rdna4Upstream));
        assert_eq!(
            resolve(Some(NrBackend::Rdna3Fork), GpuArch::Rdna4),
            Err(reason::RDNA3_ONLY)
        );
        let profile = nr_profile(NrBackend::Rdna3Fork);
        let proton = recent_proton();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna4, Some(16)), &profile, &registry())
                .with_compat(Some(&cyberpunk()))
                .with_proton(Some(&proton)),
        );
        assert_eq!(verdict.reason(), Some(reason::RDNA3_ONLY));
    }

    #[test]
    fn a_card_with_no_nr_path_is_hidden_with_the_arch_reason() {
        assert_eq!(resolve(None, GpuArch::Rdna1), Err(reason::NR_NEEDS_RDNA3_4));
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna1, Some(8)), &profile(), &registry())
                .with_compat(Some(&cyberpunk())),
        );
        assert_eq!(verdict.reason(), Some(reason::NR_NEEDS_RDNA3_4));
    }

    // -- The plan -------------------------------------------------------------

    #[test]
    fn an_rdna4_card_resolves_the_upstream_pin() {
        let proton = recent_proton();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna4, Some(16)), &profile(), &registry())
                .with_compat(Some(&cyberpunk()))
                .with_proton(Some(&proton)),
        );
        let plan = verdict.plan().expect("available");
        assert_eq!(plan.backend, NrBackend::Rdna4Upstream);
        assert!(!plan.experimental);
        assert_eq!(plan.pin.source, ReleaseSource::GitLab);
    }

    #[test]
    fn an_rdna3_card_resolves_the_experimental_fork() {
        let proton = recent_proton();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna3, Some(16)), &profile(), &registry())
                .with_compat(Some(&cyberpunk()))
                .with_proton(Some(&proton)),
        );
        let plan = verdict.plan().expect("available");
        assert_eq!(plan.backend, NrBackend::Rdna3Fork);
        assert!(plan.experimental);
        assert_eq!(plan.pin.source, ReleaseSource::GitHub);
    }

    #[test]
    fn a_fork_version_below_the_curated_minimum_is_hidden_with_a_reason() {
        // The registry's fork pin is v0.0.8; the game needs 0.0.9.
        let text = CYBERPUNK.replace(
            "dlssnr_min_version = \"0.0.8\"",
            "dlssnr_min_version = \"0.0.9\"",
        );
        let entry = CompatEntry::from_toml(&text).unwrap();
        let proton = recent_proton();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna3, Some(16)), &profile(), &registry())
                .with_compat(Some(&entry))
                .with_proton(Some(&proton)),
        );
        assert_eq!(verdict.reason(), Some(reason::PIN_BEHIND));

        // The upstream pin (v0.0.9) clears the same floor.
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna4, Some(16)), &profile(), &registry())
                .with_compat(Some(&entry))
                .with_proton(Some(&proton)),
        );
        assert!(verdict.is_available(), "{verdict:?}");
    }

    #[test]
    fn a_32_bit_game_is_hidden_even_when_the_package_is_x86_64() {
        let proton = recent_proton();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna4, Some(16)), &profile(), &registry())
                .with_compat(Some(&cyberpunk()))
                .with_proton(Some(&proton))
                .with_game_64bit(Some(false)),
        );
        assert_eq!(verdict.reason(), Some(reason::NR_32BIT));
    }

    #[test]
    fn a_backend_with_no_pin_is_hidden_by_name() {
        let empty = BackendRegistry::default();
        let proton = recent_proton();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna4, Some(16)), &profile(), &empty)
                .with_compat(Some(&cyberpunk()))
                .with_proton(Some(&proton)),
        );
        assert_eq!(verdict.reason(), Some(reason::BACKEND_NOT_PINNED));
    }

    #[test]
    fn a_game_that_is_not_curated_for_nr_is_hidden() {
        let plain = CompatEntry::from_toml(
            "[game]\nstore = \"steam\"\nstore_id = \"620\"\ntitle = \"Portal 2\"\n",
        )
        .unwrap();
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna4, Some(16)), &profile(), &registry())
                .with_compat(Some(&plain)),
        );
        assert_eq!(verdict.reason(), Some(reason::GAME_NO_NR));
    }

    #[test]
    fn an_unreadable_mesa_is_hidden_never_guessed() {
        let mut gpu = gpu(GpuArch::Rdna4, Some(16));
        gpu.mesa = None;
        let verdict =
            plan(NrRequest::new(&gpu, &profile(), &registry()).with_compat(Some(&cyberpunk())));
        assert_eq!(verdict.reason(), Some(reason::NR_MESA_UNKNOWN));
    }

    #[test]
    fn an_explicit_curated_backend_is_honoured_when_it_matches_the_card() {
        let text = CYBERPUNK.replace("backend = \"auto\"", "backend = \"rdna3_fork\"");
        let entry = CompatEntry::from_toml(&text).unwrap();
        assert_eq!(
            entry.neural_rendering.as_ref().unwrap().backend,
            NrBackendChoice::Rdna3Fork
        );
        let verdict = plan(
            NrRequest::new(&gpu(GpuArch::Rdna3, Some(16)), &profile(), &registry())
                .with_compat(Some(&entry))
                .with_proton(Some(&recent_proton())),
        );
        assert_eq!(verdict.plan().unwrap().backend, NrBackend::Rdna3Fork);
    }

    // -- The cost table and presets ------------------------------------------

    #[test]
    fn the_cost_table_matches_the_plan_numbers_at_full_scale() {
        let rdna4 = cost(NrBackend::Rdna4Upstream, Resolution::P1080, 100);
        assert!((rdna4.millis - 3.3).abs() < 0.01, "{rdna4:?}");
        assert_eq!(rdna4.vram_bytes, 650_000_000);
        let four_k = cost(NrBackend::Rdna4Upstream, Resolution::Uhd2160, 100);
        assert!((four_k.millis - 12.7).abs() < 0.01, "{four_k:?}");
        assert_eq!(four_k.vram_bytes, 2_600_000_000);

        let fork = cost(NrBackend::Rdna3Fork, Resolution::Uhd2160, 100);
        assert!((fork.millis - 56.0).abs() < 0.01, "{fork:?}");
        assert_eq!(fork.vram_bytes, 5_300_000_000);
    }

    #[test]
    fn the_cost_scales_with_the_square_of_the_model_scale() {
        // Half the network resolution is a quarter of the cost and VRAM.
        let full = cost(NrBackend::Rdna3Fork, Resolution::Uhd2160, 100);
        let half = cost(NrBackend::Rdna3Fork, Resolution::Uhd2160, 50);
        assert!((half.millis - 14.0).abs() < 0.1, "{half:?}");
        assert_eq!(half.vram_bytes, 1_325_000_000);
        assert!(half.millis < full.millis);
    }

    #[test]
    fn the_presets_default_per_resolution_and_gpu() {
        // RDNA 4 runs the full network everywhere.
        for resolution in Resolution::ALL {
            assert_eq!(
                model_scale_preset(NrBackend::Rdna4Upstream, resolution, gb(16)),
                100,
                "{resolution:?}"
            );
        }
        // The fork is full size up to 1440p and half at 4K.
        assert_eq!(
            model_scale_preset(NrBackend::Rdna3Fork, Resolution::P1080, gb(16)),
            100
        );
        assert_eq!(
            model_scale_preset(NrBackend::Rdna3Fork, Resolution::P1440, gb(16)),
            100
        );
        assert_eq!(
            model_scale_preset(NrBackend::Rdna3Fork, Resolution::Uhd2160, gb(16)),
            50
        );
    }

    #[test]
    fn a_small_card_gets_a_lower_default() {
        // The RDNA 4 4K network needs 2.6 GB; on a 4 GB card the preset steps
        // down rather than eating the whole card.
        assert_eq!(
            model_scale_preset(NrBackend::Rdna4Upstream, Resolution::Uhd2160, gb(4)),
            75
        );
        // The fork's 4K half-size network (1.33 GB) still fits a 4 GB card.
        assert_eq!(
            model_scale_preset(NrBackend::Rdna3Fork, Resolution::Uhd2160, gb(4)),
            50
        );
        // A tiny card steps the fork down too.
        assert_eq!(
            model_scale_preset(NrBackend::Rdna3Fork, Resolution::Uhd2160, gb(2)),
            25
        );
    }

    #[test]
    fn an_unknown_vram_size_applies_no_cap() {
        assert_eq!(
            model_scale_preset(NrBackend::Rdna4Upstream, Resolution::Uhd2160, None),
            100
        );
        assert_eq!(
            model_scale_preset(NrBackend::Rdna3Fork, Resolution::Uhd2160, None),
            50
        );
    }

    #[test]
    fn a_curated_scale_overrides_the_preset() {
        let entry = cyberpunk();
        // The entry pins rdna4 = 100 and rdna3 = 50.
        assert_eq!(
            default_scale(
                Some(&entry),
                NrBackend::Rdna4Upstream,
                GpuArch::Rdna4,
                Resolution::Uhd2160,
                gb(16)
            ),
            100
        );
        assert_eq!(
            default_scale(
                Some(&entry),
                NrBackend::Rdna3Fork,
                GpuArch::Rdna3,
                Resolution::Uhd2160,
                gb(16)
            ),
            50
        );
        // With no entry, the preset decides.
        assert_eq!(
            default_scale(
                None,
                NrBackend::Rdna4Upstream,
                GpuArch::Rdna4,
                Resolution::Uhd2160,
                gb(16)
            ),
            100
        );
        assert_eq!(
            curated_scale(Some(&entry), GpuArch::Rdna2),
            None,
            "an unsupported card has no curated scale"
        );
    }

    // -- The PE header check --------------------------------------------------

    /// A minimal PE32/PE32+ with the given optional-header magic.
    fn minimal_pe(magic: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x58 + 0xF0];
        bytes[0..2].copy_from_slice(b"MZ");
        // e_lfanew at 0x3C points at the PE signature.
        bytes[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        // COFF header: machine, sections, timestamp, symtab, symcount, opt size, flags.
        let machine: u16 = if magic == 0x20b { 0x8664 } else { 0x14c };
        bytes[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
        bytes[0x46..0x48].copy_from_slice(&0u16.to_le_bytes());
        bytes[0x54..0x56].copy_from_slice(&0xF0u16.to_le_bytes());
        bytes[0x56..0x58].copy_from_slice(&0x0022u16.to_le_bytes());
        // Optional header magic.
        bytes[0x58..0x5A].copy_from_slice(&magic.to_le_bytes());
        bytes
    }

    #[test]
    fn the_pe_header_check_reads_the_game_bitness() {
        let dir = tempfile::tempdir().unwrap();
        let exe64 = dir.path().join("game64.exe");
        fs::write(&exe64, minimal_pe(0x20b)).unwrap();
        assert!(pe_is_64_bit(&exe64).unwrap());

        let exe32 = dir.path().join("game32.exe");
        fs::write(&exe32, minimal_pe(0x10b)).unwrap();
        assert!(!pe_is_64_bit(&exe32).unwrap());

        let not_pe = dir.path().join("script.sh");
        fs::write(&not_pe, b"#!/bin/sh\n").unwrap();
        assert!(matches!(pe_is_64_bit(&not_pe), Err(NrError::Binary { .. })));
    }

    // -- The docs table agrees with the UI -----------------------------------

    #[test]
    fn the_docs_table_quotes_every_reason_string() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs/dlssnr.md");
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));

        for reason in REASONS {
            assert!(
                doc.contains(reason),
                "docs/dlssnr.md is missing the reason `{reason}`"
            );
        }
    }

    #[test]
    fn a_verdict_round_trips_through_json_for_the_cli() {
        let verdict = NrVerdict::Hidden {
            reason: reason::PIN_BEHIND.to_string(),
        };
        let json = serde_json::to_string(&verdict).unwrap();
        assert!(json.contains(r#""status":"hidden""#), "{json}");
        let parsed: NrVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.reason(), Some(reason::PIN_BEHIND));
    }
}
