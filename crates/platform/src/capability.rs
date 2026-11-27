//! The capability matrix: one availability verdict and one reason per field.
//!
//! Three read-only layers come together here and nowhere else:
//!
//! - [`crate::gpu`] detects the machine (architecture, Mesa, VRAM).
//! - [`crate::compat`] curates what a *game* supports (the per-game entry).
//! - [`crate::proton`] resolves the runtime, whose kind and build version gate
//!   Tier A injection and neural rendering.
//!
//! [`availability`] folds those into [`Availability`] — `Available` or
//! `Hidden { reason }`. The **same `reason` string** is the UI's "why is this
//! disabled?" copy and the docs table (`docs/m3-capability.md`), so the two
//! cannot drift. Per ADR-0004 an unsupported option is **hidden, never greyed**;
//! per ADR-0023 the single injection slot means DLSSNR and an injected upscaler
//! can never both be offered.
//!
//! The matrix is the plan §6 table (per-architecture upscaler / DLSSNR gates:
//! Mesa, Proton, 64-bit, arch) encoded as data; the compat DB stays per-game
//! curated data (ADR-0025). The per-game 64-bit check that needs the game's PE
//! header belongs to the neural-rendering backends, so here the only 64-bit gate
//! we can evaluate is the profile's `pkg_arch`.
//!
//! `envc` (M3) is the consumer: it asks [`availability`] for each field instead
//! of deciding on its own, and a `Hidden` field is never emitted. [`Field::ALL`],
//! [`matrix`], [`Field::for_upscaler`], [`Field::for_frame_gen`] and
//! [`Field::for_injection`] are the seams it needs.
//!
//! The module is framework-free and does no I/O: everything host-specific is
//! passed in, so the whole matrix is tested against fixture values with no GPU,
//! no game and no Proton installed.

use serde::{Deserialize, Serialize};

use pitagames_core::AntiCheat;
use pitagames_core::profile::{FrameGen, Injection, NeuralRendering, PkgArch, Profile, Upscaler};

use crate::compat::{CompatEntry, GameApi, HelixsrMode, Version};
use crate::gpu::{GpuArch, GpuProfile};
use crate::proton::{ProtonInstall, ProtonKind};

/// Mesa version DLSSNR (neural rendering) requires.
const NR_MESA_MIN: Version = Version {
    major: 26,
    minor: 2,
    patch: 0,
};

/// `Proton-CachyOS` build that ships the OptiScaler / FFX upgrade hooks.
///
/// The number is owned by [`crate::tier_a`] — Tier A's own gate uses it — and
/// reused here for the FFX hooks and the "recent CachyOS" check.
const CACHYOS_HOOKS_MIN: &[u64] = crate::tier_a::CACHYOS_HOOKS_MIN;

/// A recent enough GE-Proton / Valve Proton for DLSSNR and HelixSR.
const RECENT_GE_MIN: &[u64] = &[10, 0];
const RECENT_VALVE_MIN: &[u64] = &[9, 0];

// ---------------------------------------------------------------------------
// The reason strings — the single source for the UI and the docs table
// ---------------------------------------------------------------------------

/// Every "why is this hidden" string the matrix can produce.
///
/// These are `pub` so the docs (`docs/m3-capability.md`) and the tests can quote
/// the exact same text the UI shows. A test asserts every string below appears
/// verbatim in that document, so the UI copy and the docs table cannot drift.
pub mod reason {
    // The card cannot do it.
    pub const GPU_UNKNOWN: &str = "the graphics card was not recognised";
    pub const F4_NEEDS_RDNA3_PLUS: &str = "FSR 4 needs an AMD RDNA 3 or newer card";
    pub const F4_AMD_ONLY: &str = "FSR 4 is an AMD path; this card uses its own upscaler";
    pub const F4_RDNA3_ONLY: &str = "the FSR 4 RDNA 3 upgrade path is for RDNA 3 only";
    pub const OPTISCALER_AMD_ONLY: &str = "this OptiScaler path is for AMD cards";
    pub const DLSS_NEEDS_RTX: &str = "DLSS needs an NVIDIA RTX card";
    pub const XESS_NEEDS_DP4A: &str = "XeSS needs a card with DP4a support";
    pub const HELIXSR_NOT_OFFERED: &str = "HelixSR is not offered for this card";
    pub const NR_NEEDS_RDNA3_4: &str = "neural rendering needs an AMD RDNA 3 or RDNA 4 card";
    pub const MLFG_NEEDS_RDNA4: &str = "ML frame generation needs an AMD RDNA 4 card";
    pub const FG_NOT_OFFERED: &str = "frame generation is not offered for this card";
    pub const FFX_AMD_ONLY: &str = "the FFX upgrade is an AMD path";
    pub const FFX4_NEEDS_RDNA3_PLUS: &str = "the FFX 4 upgrade needs an AMD RDNA 3 or newer card";

    // The game is not curated for it.
    pub const GAME_NOT_CURATED: &str = "this game is not in the compatibility database yet";
    pub const GAME_NO_DLSS: &str = "this game does not expose DLSS";
    pub const GAME_NO_XESS: &str = "this game does not expose XeSS";
    pub const GAME_NO_HELIXSR_SCALER: &str = "HelixSR's Scaler mode is not curated for this game";
    pub const GAME_NO_HELIXSR_DROPIN: &str =
        "this game does not ship an FSR 3.1 DLL for the HelixSR drop-in";
    pub const GAME_NO_NR: &str = "neural rendering is not curated for this game";

    // The runtime is wrong or unreadable.
    pub const TIER_A_NEEDS_CACHYOS: &str = "Tier A needs Proton-CachyOS 11.0-20260506 or newer";
    pub const TIER_A_VERSION_UNREADABLE: &str = "Tier A needs Proton-CachyOS 11.0-20260506 or newer, but the build version could not be read";
    pub const TIER_A_STEAM_DEFAULT: &str =
        "Steam's default Proton cannot deploy OptiScaler; choose Proton-CachyOS";
    pub const FFX_NEEDS_CACHYOS: &str =
        "the FFX upgrade hooks need Proton-CachyOS 11.0-20260506 or newer";
    pub const NR_NEEDS_RECENT_PROTON: &str = "neural rendering needs a recent Proton or GE build";
    pub const NR_STEAM_DEFAULT_PROTON: &str =
        "neural rendering needs a recent Proton or GE build; Steam's default is not enough";
    pub const HLX_NEEDS_RECENT_PROTON: &str = "HelixSR needs a recent Proton or GE build";

    // Mesa.
    pub const NR_NEEDS_MESA: &str = "DLSSNR needs Mesa 26.2 or newer";
    pub const NR_MESA_UNKNOWN: &str =
        "DLSSNR needs Mesa 26.2 or newer, but the Mesa version could not be detected";

    // The injection slot (ADR-0023).
    pub const NR_OWNS_SLOT: &str = "DLSSNR owns the injection slot";
    pub const UPSCALER_OWNS_SLOT: &str = "an injected upscaler owns the injection slot";

    // The hard anti-cheat gate (plan §6, `platform::anticheat`).
    pub const ANTICHEAT_GATE: &str =
        "injection is off for an anti-cheat-protected game until you confirm the risk";

    // 64-bit (the game's own bitness is the neural-rendering backend's check).
    pub const NR_32BIT: &str = "DLSSNR ships x86_64 only; 32-bit games are unsupported in v1";
}

/// Every [`reason`] string, for the docs-agreement test and the CLI's `--help`.
pub const REASONS: &[&str] = &[
    reason::GPU_UNKNOWN,
    reason::F4_NEEDS_RDNA3_PLUS,
    reason::F4_AMD_ONLY,
    reason::F4_RDNA3_ONLY,
    reason::OPTISCALER_AMD_ONLY,
    reason::DLSS_NEEDS_RTX,
    reason::XESS_NEEDS_DP4A,
    reason::HELIXSR_NOT_OFFERED,
    reason::NR_NEEDS_RDNA3_4,
    reason::MLFG_NEEDS_RDNA4,
    reason::FG_NOT_OFFERED,
    reason::FFX_AMD_ONLY,
    reason::FFX4_NEEDS_RDNA3_PLUS,
    reason::GAME_NOT_CURATED,
    reason::GAME_NO_DLSS,
    reason::GAME_NO_XESS,
    reason::GAME_NO_HELIXSR_SCALER,
    reason::GAME_NO_HELIXSR_DROPIN,
    reason::GAME_NO_NR,
    reason::TIER_A_NEEDS_CACHYOS,
    reason::TIER_A_VERSION_UNREADABLE,
    reason::TIER_A_STEAM_DEFAULT,
    reason::FFX_NEEDS_CACHYOS,
    reason::NR_NEEDS_RECENT_PROTON,
    reason::NR_STEAM_DEFAULT_PROTON,
    reason::HLX_NEEDS_RECENT_PROTON,
    reason::NR_NEEDS_MESA,
    reason::NR_MESA_UNKNOWN,
    reason::NR_OWNS_SLOT,
    reason::UPSCALER_OWNS_SLOT,
    reason::ANTICHEAT_GATE,
    reason::NR_32BIT,
];

// ---------------------------------------------------------------------------
// The field
// ---------------------------------------------------------------------------

/// One graphics option the launcher can offer a game.
///
/// The set covers the plan §6 matrix (the FSR 4 paths, the OptiScaler routes,
/// HelixSR, DLSSNR, system FSR), the frame-generation toggles and the two
/// injection tiers. [`Field::ALL`] is the one list the UI, the CLI and the docs
/// table iterate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// The game's own upscaler, or none: never injects anything.
    Native,
    /// gamescope's FSR 1 fallback, present on every card.
    SystemFsr,
    /// Proton-CachyOS's `PROTON_FSR4_UPGRADE`.
    Fsr4Upgrade,
    /// The `PROTON_FSR4_RDNA3_UPGRADE` path, RDNA 3 only.
    Fsr4Rdna3Upgrade,
    /// OptiScaler presenting FSR 4 to a DLSS game.
    DlssToFsr4,
    /// OptiScaler presenting FSR 3.1 to a DLSS game.
    DlssToFsr31,
    /// OptiScaler presenting XeSS to a DLSS game.
    DlssToXess,
    /// Leave the game's native DLSS alone (the RTX path).
    DlssNative,
    /// Proton's `PROTON_XESS_UPGRADE`.
    XessUpgrade,
    /// Proton's `PROTON_FFX3_UPGRADE`.
    Ffx3Upgrade,
    /// Proton's `PROTON_FFX4_UPGRADE`.
    Ffx4Upgrade,
    /// HelixSR Scaler presenting NVIDIA's DLSS network.
    HlxDlss,
    /// HelixSR Scaler presenting HLX FSR 4.
    HlxFsr4,
    /// The HelixSR FSR 3.1 drop-in.
    HlxFsr4DropIn,
    /// DLSS 5 neural rendering on RDNA 4 / RDNA 3 (DLSSNR-AMD).
    NeuralRendering,
    /// Proton-CachyOS multi-frame generation (FSR 4).
    FrameGenFsr4Mlfg,
    /// OptiScaler-driven FSR 3 frame generation.
    FrameGenFsr3,
    /// Tier A: Proton deploys the DLLs, we set env only.
    TierAInjection,
    /// Tier B: we manage OptiScaler's files.
    TierBInjection,
}

impl Field {
    /// Every field, in the order the CLI and the docs table present them.
    pub const ALL: &'static [Field] = &[
        Field::Native,
        Field::SystemFsr,
        Field::Fsr4Upgrade,
        Field::Fsr4Rdna3Upgrade,
        Field::DlssToFsr4,
        Field::DlssToFsr31,
        Field::DlssToXess,
        Field::DlssNative,
        Field::XessUpgrade,
        Field::Ffx3Upgrade,
        Field::Ffx4Upgrade,
        Field::HlxDlss,
        Field::HlxFsr4,
        Field::HlxFsr4DropIn,
        Field::NeuralRendering,
        Field::FrameGenFsr4Mlfg,
        Field::FrameGenFsr3,
        Field::TierAInjection,
        Field::TierBInjection,
    ];

    /// The stable identifier used in JSON and in the docs table.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::SystemFsr => "system_fsr",
            Self::Fsr4Upgrade => "fsr4_upgrade",
            Self::Fsr4Rdna3Upgrade => "fsr4_rdna3_upgrade",
            Self::DlssToFsr4 => "dlss_to_fsr4",
            Self::DlssToFsr31 => "dlss_to_fsr31",
            Self::DlssToXess => "dlss_to_xess",
            Self::DlssNative => "dlss_native",
            Self::XessUpgrade => "xess_upgrade",
            Self::Ffx3Upgrade => "ffx3_upgrade",
            Self::Ffx4Upgrade => "ffx4_upgrade",
            Self::HlxDlss => "hlx_dlss",
            Self::HlxFsr4 => "hlx_fsr4",
            Self::HlxFsr4DropIn => "hlx_fsr4_drop_in",
            Self::NeuralRendering => "neural_rendering",
            Self::FrameGenFsr4Mlfg => "frame_gen_fsr4_mlfg",
            Self::FrameGenFsr3 => "frame_gen_fsr3",
            Self::TierAInjection => "tier_a_injection",
            Self::TierBInjection => "tier_b_injection",
        }
    }

    /// The human label the graphics screen shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Native => "Native (no injection)",
            Self::SystemFsr => "System FSR (gamescope)",
            Self::Fsr4Upgrade => "FSR 4 upgrade",
            Self::Fsr4Rdna3Upgrade => "FSR 4 RDNA 3 upgrade",
            Self::DlssToFsr4 => "DLSS to FSR 4 (OptiScaler)",
            Self::DlssToFsr31 => "DLSS to FSR 3.1 (OptiScaler)",
            Self::DlssToXess => "DLSS to XeSS (OptiScaler)",
            Self::DlssNative => "Native DLSS",
            Self::XessUpgrade => "XeSS upgrade",
            Self::Ffx3Upgrade => "FFX 3 upgrade",
            Self::Ffx4Upgrade => "FFX 4 upgrade",
            Self::HlxDlss => "HelixSR HLX DLSS",
            Self::HlxFsr4 => "HelixSR HLX FSR 4",
            Self::HlxFsr4DropIn => "HelixSR FSR 4 drop-in",
            Self::NeuralRendering => "DLSS 5 neural rendering (DLSSNR-AMD)",
            Self::FrameGenFsr4Mlfg => "ML frame generation (FSR 4)",
            Self::FrameGenFsr3 => "FSR 3 frame generation (OptiScaler)",
            Self::TierAInjection => "Tier A injection (env-only)",
            Self::TierBInjection => "Tier B injection (managed OptiScaler)",
        }
    }

    /// The field a profile upscaler choice asks about.
    pub const fn for_upscaler(upscaler: &Upscaler) -> Option<Self> {
        Some(match upscaler {
            Upscaler::Off | Upscaler::Native => Self::Native,
            Upscaler::Fsr4Upgrade { .. } => Self::Fsr4Upgrade,
            Upscaler::Fsr4Rdna3 { .. } => Self::Fsr4Rdna3Upgrade,
            Upscaler::DlssToFsr4 => Self::DlssToFsr4,
            Upscaler::DlssToFsr31 => Self::DlssToFsr31,
            Upscaler::DlssToXess => Self::DlssToXess,
            Upscaler::XessUpgrade => Self::XessUpgrade,
            Upscaler::Ffx3Upgrade => Self::Ffx3Upgrade,
            Upscaler::Ffx4Upgrade => Self::Ffx4Upgrade,
            Upscaler::HlxDlss => Self::HlxDlss,
            Upscaler::HlxFsr4 => Self::HlxFsr4,
            Upscaler::HlxFsr4DropIn => Self::HlxFsr4DropIn,
            Upscaler::SystemFsr => Self::SystemFsr,
        })
    }

    /// The field a frame-generation choice asks about (`Off` is no field).
    pub const fn for_frame_gen(frame_gen: FrameGen) -> Option<Self> {
        match frame_gen {
            FrameGen::Off => None,
            FrameGen::Fsr4Mlfg => Some(Self::FrameGenFsr4Mlfg),
            FrameGen::Fsr3 => Some(Self::FrameGenFsr3),
        }
    }

    /// The field an injection choice asks about.
    ///
    /// `Auto` resolves from the GPU, `Off` asks nothing, and `HelixSr` is
    /// already covered by its `Hlx*` upscaler field.
    pub const fn for_injection(injection: Injection) -> Option<Self> {
        match injection {
            Injection::Auto | Injection::Off | Injection::HelixSr => None,
            Injection::EnvOnly => Some(Self::TierAInjection),
            Injection::Manual => Some(Self::TierBInjection),
            Injection::DlssNr => Some(Self::NeuralRendering),
        }
    }

    /// Whether this field takes the single injection slot (ADR-0023).
    ///
    /// DLSSNR owns the slot, so any of these is hidden while it is enabled;
    /// `Native`, `SystemFsr` and `DlssNative` inject nothing.
    const fn uses_slot(self) -> bool {
        !matches!(self, Self::Native | Self::SystemFsr | Self::DlssNative)
            && !matches!(self, Self::NeuralRendering)
    }

    /// Whether this field puts code or a DLL into the game's process.
    ///
    /// It is the set the hard anti-cheat gate hides: everything except the
    /// game's own upscaler/API and gamescope's compositor FSR. Unlike
    /// [`Field::uses_slot`] it includes neural rendering, which injects too.
    pub const fn is_injection(self) -> bool {
        !matches!(self, Self::Native | Self::SystemFsr | Self::DlssNative)
    }

    /// Whether the game has to expose the API this field hooks into.
    const fn needs_game_api(self) -> bool {
        matches!(
            self,
            Self::DlssToFsr4
                | Self::DlssToFsr31
                | Self::DlssToXess
                | Self::DlssNative
                | Self::XessUpgrade
                | Self::HlxDlss
                | Self::HlxFsr4
                | Self::HlxFsr4DropIn
                | Self::NeuralRendering
        )
    }
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// Whether the machine may offer a field, and — when it may not — why.
///
/// The `reason` is the UI's "why disabled" copy and the docs table's "why"
/// column, from the same string. An option is hidden, never greyed (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Availability {
    Available,
    Hidden { reason: String },
}

impl Availability {
    /// The option is offered.
    pub fn available() -> Self {
        Self::Available
    }

    /// The option is hidden, with the one-line reason.
    pub fn hidden(reason: impl Into<String>) -> Self {
        Self::Hidden {
            reason: reason.into(),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// The reason, when hidden.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Hidden { reason } => Some(reason),
        }
    }
}

// ---------------------------------------------------------------------------
// The hard anti-cheat gate
// ---------------------------------------------------------------------------

/// The anti-cheat state the injection gate acts on, and whether the user has
/// recorded a confirmation for it.
///
/// This is the compiler-level half of `platform::anticheat`'s advisory gate: a
/// protected game hides every injection field ([`Availability::Hidden`] with
/// [`reason::ANTICHEAT_GATE`]) until [`Self::confirmed`] is set, so `envc`
/// refuses to emit injection env for a protected game the same way the graphics
/// screen hides it. `Unknown` never exits [`Self::protected`], so a game whose
/// anti-cheat we simply have not catalogued is never hard-gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InjectionGate {
    /// The effective anti-cheat: detection merged with the compat DB (the app
    /// writes the merged value onto the entry it passes in), or the curated
    /// value when only curation is available.
    pub anticheat: AntiCheat,
    /// Whether the game is curated as EAC Linux v2, which is Easy Anti-Cheat
    /// even when the runtime field is unset.
    pub eac_v2: bool,
    /// Whether the profile records the user's explicit confirmation.
    pub confirmed: bool,
}

impl InjectionGate {
    /// The gate for an effective anti-cheat value and a recorded confirmation.
    pub const fn new(anticheat: AntiCheat, eac_v2: bool, confirmed: bool) -> Self {
        Self {
            anticheat,
            eac_v2,
            confirmed,
        }
    }

    /// The gate a compat entry and a recorded confirmation imply.
    ///
    /// The entry is the one the caller passes to [`availability`]; a caller that
    /// has a detected anti-cheat value writes it onto the entry first, so this
    /// is the effective state, not only the curated one.
    pub fn from_compat(compat: Option<&CompatEntry>, confirmed: bool) -> Self {
        Self {
            anticheat: compat.map(|entry| entry.anticheat).unwrap_or_default(),
            eac_v2: compat.is_some_and(|entry| entry.eac_v2),
            confirmed,
        }
    }

    /// The anti-cheat value the gate acts on (an `eac_v2` claim reads as EAC).
    pub fn effective(&self) -> AntiCheat {
        crate::anticheat::protected_kind(self.anticheat, self.eac_v2)
    }

    /// Whether the game is protected by a concrete anti-cheat runtime.
    pub fn protected(&self) -> bool {
        self.effective().is_protected()
    }

    /// Whether the gate refuses injection: protected *and* unconfirmed.
    pub fn refuses(&self) -> bool {
        self.protected() && !self.confirmed
    }
}

// ---------------------------------------------------------------------------
// The verdict function
// ---------------------------------------------------------------------------

/// Decide whether `field` may be offered on this machine, for this game.
///
/// `profile` supplies the single injection slot's current owner and, for neural
/// rendering, the package architecture. `gpu` is the detected card, `compat` the
/// game's curated entry (`None` when the game is not in the database) and
/// `proton` the *resolved* build (`None` when Steam decides).
///
/// The verdict is `Available` or `Hidden { reason }`; the reason is the same
/// string the UI shows and the docs table quotes.
pub fn availability(
    field: Field,
    profile: &Profile,
    gpu: &GpuProfile,
    compat: Option<&CompatEntry>,
    proton: Option<&ProtonInstall>,
) -> Availability {
    // 0. The hard anti-cheat gate. A protected game hides every injection field
    //    until the profile records a confirmation; `Unknown` never gates. It
    //    runs first so the reader always sees the safety-critical reason, and
    //    `envc` consumes the same verdict to refuse injection env.
    if field.is_injection() {
        let gate = InjectionGate::from_compat(compat, profile.injection_confirmed);
        if gate.refuses() {
            return Availability::hidden(reason::ANTICHEAT_GATE);
        }
    }

    // 1. The card itself.
    let arch = arch_basis(field, gpu.arch);
    if !arch.is_available() {
        return arch;
    }

    // 2. The single injection slot (ADR-0023).
    if field.uses_slot() && profile.neural_rendering.is_enabled() {
        return Availability::hidden(reason::NR_OWNS_SLOT);
    }
    if field == Field::NeuralRendering && other_slot_owner(profile) {
        return Availability::hidden(reason::UPSCALER_OWNS_SLOT);
    }

    // 3. Whether the game is curated for it.
    if let Some(reason) = game_gate(field, compat) {
        return Availability::hidden(reason);
    }

    // 4. The 64-bit gate we can evaluate without the game binary: the profile's
    //    own DLSSNR package architecture. (The game's PE header is the neural
    //    rendering backends' check.)
    if field == Field::NeuralRendering
        && matches!(
            profile.neural_rendering,
            NeuralRendering::DlssNrAmd {
                pkg_arch: PkgArch::I686,
                ..
            }
        )
    {
        return Availability::hidden(reason::NR_32BIT);
    }

    // 5. Mesa.
    if field == Field::NeuralRendering
        && let Some(reason) = mesa_gate(gpu)
    {
        return Availability::hidden(reason);
    }

    // 6. Proton.
    if let Some(reason) = proton_gate(field, proton) {
        return Availability::hidden(reason);
    }

    Availability::Available
}

/// Every field's verdict for one machine and game, in [`Field::ALL`] order.
///
/// This is the source the CLI table and the docs matrix render.
pub fn matrix(
    profile: &Profile,
    gpu: &GpuProfile,
    compat: Option<&CompatEntry>,
    proton: Option<&ProtonInstall>,
) -> Vec<(Field, Availability)> {
    Field::ALL
        .iter()
        .map(|field| (*field, availability(*field, profile, gpu, compat, proton)))
        .collect()
}

/// Whether the profile already has an injection-slot owner other than DLSSNR.
fn other_slot_owner(profile: &Profile) -> bool {
    profile.upscaler.uses_injection()
        || profile.frame_gen != FrameGen::Off
        || matches!(
            profile.injection,
            Injection::EnvOnly | Injection::Manual | Injection::HelixSr
        )
}

// ---------------------------------------------------------------------------
// The architecture table (plan §6)
// ---------------------------------------------------------------------------

/// The plan §6 matrix: what an architecture can do for a field.
///
/// `Available` covers both the "yes" and the "experimental" cells — an
/// experimental option is still offered, marked "not tested by us" (ADR-0004).
/// A "no" / "n/a" cell is the `Hidden` reason.
fn arch_basis(field: Field, arch: GpuArch) -> Availability {
    use GpuArch::*;

    // These depend on no detected architecture, so an unrecognised card (or no
    // card at all) still offers them.
    if matches!(field, Field::Native | Field::SystemFsr) {
        return Availability::Available;
    }

    if arch == Unknown {
        return Availability::hidden(reason::GPU_UNKNOWN);
    }

    match field {
        Field::Native | Field::SystemFsr | Field::TierAInjection | Field::TierBInjection => {
            Availability::Available
        }
        Field::Fsr4Upgrade => match arch {
            Rdna4 => Availability::Available,
            Rdna3 => Availability::hidden(reason::F4_RDNA3_ONLY),
            Rdna1 | Rdna2 => Availability::hidden(reason::F4_NEEDS_RDNA3_PLUS),
            _ => Availability::hidden(reason::F4_AMD_ONLY),
        },
        Field::Fsr4Rdna3Upgrade => match arch {
            Rdna3 => Availability::Available,
            _ => Availability::hidden(reason::F4_RDNA3_ONLY),
        },
        Field::DlssToFsr4 => match arch {
            Rdna4 | Rdna3 => Availability::Available,
            Rdna1 | Rdna2 => Availability::hidden(reason::F4_NEEDS_RDNA3_PLUS),
            _ => Availability::hidden(reason::OPTISCALER_AMD_ONLY),
        },
        Field::DlssToFsr31 => Availability::Available,
        Field::DlssToXess => match arch {
            Gtx10 | Gtx16 => Availability::hidden(reason::XESS_NEEDS_DP4A),
            _ => Availability::Available,
        },
        Field::DlssNative => match arch {
            Rtx20 | Rtx30 | Rtx40 | Rtx50 => Availability::Available,
            _ => Availability::hidden(reason::DLSS_NEEDS_RTX),
        },
        Field::XessUpgrade => match arch {
            Dg2 | Bmg => Availability::Available,
            Gtx10 => Availability::hidden(reason::XESS_NEEDS_DP4A),
            _ => Availability::Available,
        },
        Field::Ffx3Upgrade => match arch {
            Rdna1 | Rdna2 | Rdna3 | Rdna4 => Availability::Available,
            _ => Availability::hidden(reason::FFX_AMD_ONLY),
        },
        Field::Ffx4Upgrade => match arch {
            Rdna4 => Availability::Available,
            Rdna3 => Availability::Available,
            Rdna1 | Rdna2 => Availability::hidden(reason::FFX4_NEEDS_RDNA3_PLUS),
            _ => Availability::hidden(reason::FFX_AMD_ONLY),
        },
        Field::HlxDlss | Field::HlxFsr4 | Field::HlxFsr4DropIn => match arch {
            Rdna1 | Rdna2 | Rdna3 | Rdna4 | Rtx20 | Rtx30 | Rtx40 | Rtx50 => {
                Availability::Available
            }
            Dg2 | Bmg => Availability::Available,
            Gtx16 => Availability::Available,
            Gtx10 | Unknown => Availability::hidden(reason::HELIXSR_NOT_OFFERED),
        },
        Field::NeuralRendering => match arch {
            Rdna4 | Rdna3 => Availability::Available,
            _ => Availability::hidden(reason::NR_NEEDS_RDNA3_4),
        },
        Field::FrameGenFsr4Mlfg => match arch {
            Rdna4 => Availability::Available,
            _ => Availability::hidden(reason::MLFG_NEEDS_RDNA4),
        },
        Field::FrameGenFsr3 => match arch {
            Gtx10 | Gtx16 => Availability::hidden(reason::FG_NOT_OFFERED),
            _ => Availability::Available,
        },
    }
}

// ---------------------------------------------------------------------------
// The game gate
// ---------------------------------------------------------------------------

/// Whether the compat entry lets the game offer `field`.
///
/// A game the database does not know is `GAME_NOT_CURATED` for every
/// game-dependent route; the hardware-only paths (`Fsr4Upgrade`, `SystemFsr`, …)
/// stay available.
fn game_gate(field: Field, compat: Option<&CompatEntry>) -> Option<&'static str> {
    if !field.needs_game_api() {
        return None;
    }
    let Some(entry) = compat else {
        return Some(reason::GAME_NOT_CURATED);
    };

    let supported = |api: GameApi| entry.upscaling.supported.contains(&api);
    let has_helixsr = |mode: HelixsrMode| {
        entry
            .upscaling
            .helixsr
            .as_ref()
            .is_some_and(|plan| plan.modes.contains(&mode))
    };

    match field {
        Field::DlssToFsr4 | Field::DlssToFsr31 | Field::DlssToXess | Field::DlssNative => {
            (!supported(GameApi::Dlss)).then_some(reason::GAME_NO_DLSS)
        }
        Field::XessUpgrade => (!supported(GameApi::Xess)).then_some(reason::GAME_NO_XESS),
        Field::HlxDlss | Field::HlxFsr4 => {
            (!has_helixsr(HelixsrMode::Scaler)).then_some(reason::GAME_NO_HELIXSR_SCALER)
        }
        Field::HlxFsr4DropIn => {
            (!has_helixsr(HelixsrMode::Fsr31Dropin)).then_some(reason::GAME_NO_HELIXSR_DROPIN)
        }
        Field::NeuralRendering => entry
            .neural_rendering
            .is_none()
            .then_some(reason::GAME_NO_NR),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The Mesa gate
// ---------------------------------------------------------------------------

/// Whether the detected Mesa is new enough for DLSSNR.
///
/// An unreadable Mesa version (`None`) hides the option too — never guesses
/// (plan §6: "an older or unreadable version hides Tier A with a reason").
fn mesa_gate(gpu: &GpuProfile) -> Option<&'static str> {
    match gpu.mesa {
        None => Some(reason::NR_MESA_UNKNOWN),
        Some(version) if version < NR_MESA_MIN => Some(reason::NR_NEEDS_MESA),
        Some(_) => None,
    }
}

// ---------------------------------------------------------------------------
// The Proton gate
// ---------------------------------------------------------------------------

/// Whether the resolved runtime satisfies `field`'s Proton requirement.
fn proton_gate(field: Field, proton: Option<&ProtonInstall>) -> Option<&'static str> {
    match field {
        Field::TierAInjection => tier_a_gate(proton),
        Field::Ffx3Upgrade | Field::Ffx4Upgrade => {
            cachyos_hooks(proton, reason::FFX_NEEDS_CACHYOS, reason::FFX_NEEDS_CACHYOS)
        }
        Field::HlxDlss | Field::HlxFsr4 | Field::HlxFsr4DropIn => {
            recent_proton(proton, reason::HLX_NEEDS_RECENT_PROTON)
        }
        Field::NeuralRendering => nr_proton(proton),
        _ => None,
    }
}

/// Neural rendering's own Proton reasons (a missing runtime and an old one read
/// differently).
fn nr_proton(proton: Option<&ProtonInstall>) -> Option<&'static str> {
    match proton {
        None => Some(reason::NR_STEAM_DEFAULT_PROTON),
        Some(install) if proton_is_recent(install) => None,
        Some(_) => Some(reason::NR_NEEDS_RECENT_PROTON),
    }
}

/// Tier A's own Proton gate, mapped to the reason the UI shows.
///
/// The verdict comes from [`crate::tier_a::gate`] — the one place that parses
/// the resolved build's metadata — so the Graphics screen, this matrix and the
/// compiler cannot disagree about whether Tier A is offered.
fn tier_a_gate(proton: Option<&ProtonInstall>) -> Option<&'static str> {
    match crate::tier_a::gate(proton) {
        crate::tier_a::TierAGate::Supported => None,
        crate::tier_a::TierAGate::SteamDefault => Some(reason::TIER_A_STEAM_DEFAULT),
        crate::tier_a::TierAGate::Unreadable => Some(reason::TIER_A_VERSION_UNREADABLE),
        // An older CachyOS build and a non-CachyOS build read the same way: the
        // user needs the CachyOS build at the hook version.
        crate::tier_a::TierAGate::TooOld | crate::tier_a::TierAGate::OtherBuild => {
            Some(reason::TIER_A_NEEDS_CACHYOS)
        }
    }
}

/// `Proton-CachyOS` at the version that ships the upgrade hooks.
fn cachyos_hooks(
    proton: Option<&ProtonInstall>,
    steam_default: &'static str,
    needs_cachyos: &'static str,
) -> Option<&'static str> {
    match proton {
        None => Some(steam_default),
        Some(install) if proton_kind_is_cachyos_with_hooks(install) => None,
        Some(_) => Some(needs_cachyos),
    }
}

/// A recent enough Proton or GE build for DLSSNR / HelixSR.
fn recent_proton(proton: Option<&ProtonInstall>, reason: &'static str) -> Option<&'static str> {
    match proton {
        Some(install) if proton_is_recent(install) => None,
        _ => Some(reason),
    }
}

fn proton_kind_is_cachyos_with_hooks(install: &ProtonInstall) -> bool {
    install.kind == ProtonKind::CachyOs && install.version_key().as_slice() >= CACHYOS_HOOKS_MIN
}

fn proton_is_recent(install: &ProtonInstall) -> bool {
    let floor: &[u64] = match install.kind {
        ProtonKind::CachyOs => CACHYOS_HOOKS_MIN,
        ProtonKind::Ge => RECENT_GE_MIN,
        ProtonKind::Valve | ProtonKind::Experimental | ProtonKind::Umu => RECENT_VALVE_MIN,
        // An unclassifiable directory is never claimed to be recent.
        ProtonKind::Other => return false,
    };
    install.version_key().as_slice() >= floor
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{GameId, Store};
    use std::path::PathBuf;

    const CYBERPUNK: &str = r#"
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
anticheat = "none"
verified = ["rdna4"]

[upscaling]
supported = ["dlss", "fsr3", "xess"]
preferred_dll = "dbghelp.dll"
helixsr = { modes = ["scaler", "fsr31_dropin"], preferred = "scaler" }

[neural_rendering]
backend = "auto"
route = "optiscaler"
min_mesa = "26.2"
model_scale_default = { rdna4 = 100, rdna3 = 50 }
requires_64bit = true
"#;

    /// A game with no upscaler hooks at all.
    const PLAIN: &str = r#"
[game]
store = "steam"
store_id = "620"
title = "Portal 2"
"#;

    fn game() -> GameId {
        GameId::new(Store::Steam, "1091500")
    }

    fn profile() -> Profile {
        Profile::new(game(), "capability")
    }

    fn gpu(arch: GpuArch) -> GpuProfile {
        GpuProfile {
            vendor: arch.vendor(),
            arch,
            device_id: None,
            name: None,
            mesa: Some(Version::parse("27.0.0").unwrap()),
            vram_bytes: None,
            ntsync: false,
            hdr: crate::gpu::HdrSupport::Unknown,
            warnings: Vec::new(),
        }
    }

    fn cachyos(version: &str) -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
            name: "proton-cachyos".to_string(),
            kind: ProtonKind::CachyOs,
            version: Some(format!("proton-cachyos {version}")),
            source: crate::proton::ProtonSource::SystemCompatibilityTools,
        }
    }

    fn ge(version: &str) -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/steam/compatibilitytools.d/GE-Proton"),
            name: "GE-Proton".to_string(),
            kind: ProtonKind::Ge,
            version: Some(format!("GE-Proton{version}")),
            source: crate::proton::ProtonSource::UserCompatibilityTools,
        }
    }

    fn cyberpunk() -> CompatEntry {
        CompatEntry::from_toml(CYBERPUNK).unwrap()
    }

    fn plain() -> CompatEntry {
        CompatEntry::from_toml(PLAIN).unwrap()
    }

    fn verdict(field: Field, arch: GpuArch) -> Availability {
        availability(
            field,
            &profile(),
            &gpu(arch),
            Some(&cyberpunk()),
            Some(&cachyos("11.0-20260516")),
        )
    }

    // -- The plan §6 matrix, per architecture -------------------------------

    #[test]
    fn every_field_resolves_to_available_or_a_reason() {
        for arch in [
            GpuArch::Rdna4,
            GpuArch::Rdna3,
            GpuArch::Rdna2,
            GpuArch::Rdna1,
            GpuArch::Dg2,
            GpuArch::Bmg,
            GpuArch::Rtx50,
            GpuArch::Rtx40,
            GpuArch::Rtx30,
            GpuArch::Rtx20,
            GpuArch::Gtx16,
            GpuArch::Gtx10,
            GpuArch::Unknown,
        ] {
            for field in Field::ALL {
                let verdict = verdict(*field, arch);
                match &verdict {
                    Availability::Available => {}
                    Availability::Hidden { reason } => {
                        assert!(
                            !reason.trim().is_empty(),
                            "{field:?} on {arch:?} is hidden without a reason"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rdna4_is_the_validated_primary_and_gates_the_rdna3_path() {
        assert!(verdict(Field::Native, GpuArch::Rdna4).is_available());
        assert!(verdict(Field::SystemFsr, GpuArch::Rdna4).is_available());
        assert!(verdict(Field::Fsr4Upgrade, GpuArch::Rdna4).is_available());
        assert!(verdict(Field::DlssToFsr4, GpuArch::Rdna4).is_available());
        assert!(verdict(Field::NeuralRendering, GpuArch::Rdna4).is_available());
        assert!(
            !verdict(Field::Fsr4Rdna3Upgrade, GpuArch::Rdna4).is_available(),
            "the RDNA 3 path is not offered on RDNA 4"
        );
        assert_eq!(
            verdict(Field::Fsr4Rdna3Upgrade, GpuArch::Rdna4)
                .reason()
                .unwrap(),
            reason::F4_RDNA3_ONLY
        );
    }

    #[test]
    fn rdna1_keeps_system_fsr_and_helixsr_but_not_fsr4_or_neural_rendering() {
        assert!(verdict(Field::SystemFsr, GpuArch::Rdna1).is_available());
        assert!(verdict(Field::HlxDlss, GpuArch::Rdna1).is_available());
        assert!(verdict(Field::HlxFsr4, GpuArch::Rdna1).is_available());
        assert_eq!(
            verdict(Field::Fsr4Upgrade, GpuArch::Rdna1)
                .reason()
                .unwrap(),
            reason::F4_NEEDS_RDNA3_PLUS
        );
        assert_eq!(
            verdict(Field::NeuralRendering, GpuArch::Rdna1)
                .reason()
                .unwrap(),
            reason::NR_NEEDS_RDNA3_4
        );
    }

    #[test]
    fn rdna3_gets_the_experimental_fsr4_paths_and_the_fork() {
        assert!(verdict(Field::Fsr4Rdna3Upgrade, GpuArch::Rdna3).is_available());
        assert!(verdict(Field::DlssToFsr4, GpuArch::Rdna3).is_available());
        assert!(verdict(Field::NeuralRendering, GpuArch::Rdna3).is_available());
        // But not the plain RDNA 4 upgrade, which needs the RDNA 3 flag.
        assert_eq!(
            verdict(Field::Fsr4Upgrade, GpuArch::Rdna3)
                .reason()
                .unwrap(),
            reason::F4_RDNA3_ONLY
        );
    }

    #[test]
    fn nvidia_gets_native_dlss_and_not_the_amd_paths() {
        assert!(verdict(Field::DlssNative, GpuArch::Rtx40).is_available());
        assert!(verdict(Field::HlxDlss, GpuArch::Rtx40).is_available());
        assert_eq!(
            verdict(Field::Fsr4Upgrade, GpuArch::Rtx40)
                .reason()
                .unwrap(),
            reason::F4_AMD_ONLY
        );
        assert_eq!(
            verdict(Field::NeuralRendering, GpuArch::Rtx40)
                .reason()
                .unwrap(),
            reason::NR_NEEDS_RDNA3_4
        );
        // A GTX card has no DLSS either.
        assert_eq!(
            verdict(Field::DlssNative, GpuArch::Gtx16).reason().unwrap(),
            reason::DLSS_NEEDS_RTX
        );
    }

    #[test]
    fn intel_gets_xess_and_is_never_offered_dlss() {
        assert!(verdict(Field::XessUpgrade, GpuArch::Bmg).is_available());
        assert!(verdict(Field::XessUpgrade, GpuArch::Dg2).is_available());
        assert_eq!(
            verdict(Field::DlssNative, GpuArch::Dg2).reason().unwrap(),
            reason::DLSS_NEEDS_RTX
        );
    }

    #[test]
    fn an_unrecognised_card_hides_everything_gated() {
        assert_eq!(
            verdict(Field::Fsr4Upgrade, GpuArch::Unknown)
                .reason()
                .unwrap(),
            reason::GPU_UNKNOWN
        );
        // gamescope's own FSR does not depend on the card we recognised.
        assert!(verdict(Field::SystemFsr, GpuArch::Unknown).is_available());
    }

    // -- The game gate ------------------------------------------------------

    #[test]
    fn a_game_the_database_does_not_know_hides_every_game_dependent_route() {
        let entryless = availability(
            Field::DlssToFsr4,
            &profile(),
            &gpu(GpuArch::Rdna4),
            None,
            Some(&cachyos("11.0-20260516")),
        );
        assert_eq!(entryless.reason().unwrap(), reason::GAME_NOT_CURATED);

        // The hardware-only path stays available without a curated entry.
        assert!(
            availability(
                Field::Fsr4Upgrade,
                &profile(),
                &gpu(GpuArch::Rdna4),
                None,
                Some(&cachyos("11.0-20260516")),
            )
            .is_available()
        );
    }

    #[test]
    fn a_curated_game_that_lacks_the_api_hides_the_route_with_a_reason() {
        let entry = plain();
        // Each field is asked on an architecture where the card itself permits
        // it, so the game gate is the first blocker (gates run arch → game).
        for (field, arch, expected) in [
            (Field::DlssToFsr4, GpuArch::Rdna4, reason::GAME_NO_DLSS),
            (Field::DlssNative, GpuArch::Rtx40, reason::GAME_NO_DLSS),
            (Field::XessUpgrade, GpuArch::Bmg, reason::GAME_NO_XESS),
            (
                Field::HlxDlss,
                GpuArch::Rdna1,
                reason::GAME_NO_HELIXSR_SCALER,
            ),
            (Field::NeuralRendering, GpuArch::Rdna4, reason::GAME_NO_NR),
        ] {
            let verdict = availability(
                field,
                &profile(),
                &gpu(arch),
                Some(&entry),
                Some(&cachyos("11.0-20260516")),
            );
            assert_eq!(verdict.reason(), Some(expected), "{field:?}");
        }
    }

    #[test]
    fn the_helixsr_drop_in_needs_the_fsr31_mode() {
        let entry = compat_with(Some(r#"helixsr = { modes = ["scaler"] }"#));
        assert_eq!(
            availability(
                Field::HlxFsr4DropIn,
                &profile(),
                &gpu(GpuArch::Rdna1),
                Some(&entry),
                None,
            )
            .reason(),
            Some(reason::GAME_NO_HELIXSR_DROPIN)
        );
    }

    fn compat_with(upscaling: Option<&str>) -> CompatEntry {
        let section = match upscaling {
            Some(extra) => format!("supported = [\"dlss\"]\n{extra}\n"),
            None => "supported = [\"dlss\"]\n".to_string(),
        };
        CompatEntry::from_toml(&format!(
            "[game]\nstore = \"steam\"\nstore_id = \"1\"\ntitle = \"x\"\n\n[upscaling]\n{section}"
        ))
        .unwrap()
    }

    // -- The Mesa and Proton gates ------------------------------------------

    #[test]
    fn neural_rendering_needs_a_mesa_new_enough_or_a_readable_one() {
        let mut old = gpu(GpuArch::Rdna4);
        old.mesa = Some(Version::parse("25.3.0").unwrap());
        assert_eq!(
            availability(
                Field::NeuralRendering,
                &profile(),
                &old,
                Some(&cyberpunk()),
                Some(&cachyos("11.0-20260516")),
            )
            .reason(),
            Some(reason::NR_NEEDS_MESA)
        );

        let mut unknown = gpu(GpuArch::Rdna4);
        unknown.mesa = None;
        assert_eq!(
            availability(
                Field::NeuralRendering,
                &profile(),
                &unknown,
                Some(&cyberpunk()),
                Some(&cachyos("11.0-20260516")),
            )
            .reason(),
            Some(reason::NR_MESA_UNKNOWN)
        );
    }

    #[test]
    fn a_newer_mesa_and_proton_unlock_the_matrix() {
        // The old machine: Mesa too old and an old build.
        let mut old = gpu(GpuArch::Rdna4);
        old.mesa = Some(Version::parse("25.3.0").unwrap());
        assert!(
            !availability(
                Field::NeuralRendering,
                &profile(),
                &old,
                Some(&cyberpunk()),
                Some(&cachyos("10.0-20251101")),
            )
            .is_available()
        );

        // A newer Mesa alone is not enough: the Proton must be recent too.
        let recent_mesa = gpu(GpuArch::Rdna4);
        assert_eq!(
            availability(
                Field::NeuralRendering,
                &profile(),
                &recent_mesa,
                Some(&cyberpunk()),
                Some(&cachyos("10.0-20251101")),
            )
            .reason(),
            Some(reason::NR_NEEDS_RECENT_PROTON)
        );

        // A newer Mesa and a newer build unlock it.
        assert!(
            availability(
                Field::NeuralRendering,
                &profile(),
                &recent_mesa,
                Some(&cyberpunk()),
                Some(&ge("10-1")),
            )
            .is_available()
        );
    }

    #[test]
    fn tier_a_needs_the_cachyos_build_at_the_hook_version() {
        assert!(
            availability(
                Field::TierAInjection,
                &profile(),
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&cachyos("11.0-20260506")),
            )
            .is_available()
        );
        assert_eq!(
            availability(
                Field::TierAInjection,
                &profile(),
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&cachyos("10.0-20251101")),
            )
            .reason(),
            Some(reason::TIER_A_NEEDS_CACHYOS)
        );
        assert_eq!(
            availability(
                Field::TierAInjection,
                &profile(),
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&ge("10-5")),
            )
            .reason(),
            Some(reason::TIER_A_NEEDS_CACHYOS)
        );
        // Steam decides: we cannot know the build.
        assert_eq!(
            availability(
                Field::TierAInjection,
                &profile(),
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                None,
            )
            .reason(),
            Some(reason::TIER_A_STEAM_DEFAULT)
        );
    }

    #[test]
    fn an_unreadable_tier_a_version_hides_tier_a_and_offers_tier_b() {
        // A CachyOS build whose `version` file could not be read is never
        // assumed to be new enough; Tier B is the fallback the UI offers.
        let unreadable = ProtonInstall {
            version: None,
            ..cachyos("11.0-20260516")
        };
        assert_eq!(
            availability(
                Field::TierAInjection,
                &profile(),
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&unreadable),
            )
            .reason(),
            Some(reason::TIER_A_VERSION_UNREADABLE)
        );
        assert!(
            availability(
                Field::TierBInjection,
                &profile(),
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&unreadable),
            )
            .is_available()
        );
    }

    // -- The injection slot and the 64-bit gate -----------------------------

    #[test]
    fn dlssnr_owns_the_slot_so_an_injected_upscaler_is_hidden() {
        let mut profile = profile();
        profile.neural_rendering = nr(pkg_arch_x86_64());
        let verdict = availability(
            Field::DlssToFsr4,
            &profile,
            &gpu(GpuArch::Rdna4),
            Some(&cyberpunk()),
            Some(&cachyos("11.0-20260516")),
        );
        assert_eq!(verdict.reason(), Some(reason::NR_OWNS_SLOT));

        // Neural rendering itself stays available in that profile.
        assert!(
            availability(
                Field::NeuralRendering,
                &profile,
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&cachyos("11.0-20260516")),
            )
            .is_available()
        );
    }

    #[test]
    fn an_injected_upscaler_owns_the_slot_so_neural_rendering_is_hidden() {
        let mut profile = profile();
        profile.upscaler = Upscaler::DlssToFsr4;
        assert_eq!(
            availability(
                Field::NeuralRendering,
                &profile,
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&cachyos("11.0-20260516")),
            )
            .reason(),
            Some(reason::UPSCALER_OWNS_SLOT)
        );
    }

    #[test]
    fn a_32_bit_dlssnr_package_is_hidden() {
        let mut profile = profile();
        profile.neural_rendering = nr(PkgArch::I686);
        assert_eq!(
            availability(
                Field::NeuralRendering,
                &profile,
                &gpu(GpuArch::Rdna4),
                Some(&cyberpunk()),
                Some(&cachyos("11.0-20260516")),
            )
            .reason(),
            Some(reason::NR_32BIT)
        );
    }

    // -- The hard anti-cheat gate -------------------------------------------

    /// The curated entry with its `anticheat` field replaced, as the app's
    /// effective entry would read.
    fn entry_with_anticheat(value: &str) -> CompatEntry {
        CompatEntry::from_toml(
            &CYBERPUNK.replace("anticheat = \"none\"", &format!("anticheat = \"{value}\"")),
        )
        .unwrap()
    }

    /// A curated entry claiming EAC.
    fn protected_entry() -> CompatEntry {
        entry_with_anticheat("eac")
    }

    #[test]
    fn a_protected_game_hides_every_injection_field_until_confirmed() {
        let entry = protected_entry();
        let gpu = gpu(GpuArch::Rdna4);
        let proton = cachyos("11.0-20260516");
        let unconfirmed = profile();

        // Every injection field is hidden with the gate reason…
        for field in Field::ALL.iter().copied().filter(|f| f.is_injection()) {
            let verdict = availability(field, &unconfirmed, &gpu, Some(&entry), Some(&proton));
            assert_eq!(
                verdict.reason(),
                Some(reason::ANTICHEAT_GATE),
                "{field:?} must be gated"
            );
        }
        // …while the fields that inject nothing are not gated (they may still
        // be hidden for another reason, e.g. `DlssNative` on an AMD card).
        for field in [Field::Native, Field::SystemFsr, Field::DlssNative] {
            assert_ne!(
                availability(field, &unconfirmed, &gpu, Some(&entry), Some(&proton)).reason(),
                Some(reason::ANTICHEAT_GATE),
                "{field:?} injects nothing and must not be gated"
            );
        }

        // A recorded confirmation unlocks injection again.
        let mut confirmed = profile();
        confirmed.injection_confirmed = true;
        assert!(
            availability(
                Field::TierBInjection,
                &confirmed,
                &gpu,
                Some(&entry),
                Some(&proton),
            )
            .is_available()
        );
    }

    #[test]
    fn an_eac_v2_claim_is_protected_even_without_the_anticheat_field() {
        // The curated `anticheat` field is left at its default (`none`); the
        // `eac_v2` claim alone protects the game.
        let entry = CompatEntry::from_toml(&CYBERPUNK.replace(
            "anticheat = \"none\"",
            "anticheat = \"none\"\neac_v2 = true",
        ))
        .unwrap();
        assert_eq!(entry.anticheat, AntiCheat::None);
        let verdict = availability(
            Field::TierBInjection,
            &profile(),
            &gpu(GpuArch::Rdna4),
            Some(&entry),
            Some(&cachyos("11.0-20260516")),
        );
        assert_eq!(verdict.reason(), Some(reason::ANTICHEAT_GATE));
    }

    #[test]
    fn unknown_anti_cheat_never_hard_gates_injection() {
        for value in ["unknown", "none"] {
            let entry = entry_with_anticheat(value);
            assert!(
                availability(
                    Field::TierBInjection,
                    &profile(),
                    &gpu(GpuArch::Rdna4),
                    Some(&entry),
                    Some(&cachyos("11.0-20260516")),
                )
                .is_available(),
                "{value} must not gate"
            );
        }
        // No compat entry at all is the uncurated case: not protected.
        assert!(
            availability(
                Field::TierBInjection,
                &profile(),
                &gpu(GpuArch::Rdna4),
                None,
                Some(&cachyos("11.0-20260516")),
            )
            .is_available()
        );
    }

    #[test]
    fn the_gate_reads_the_effective_anticheat_the_entry_carries() {
        // The app writes a detected marker onto the entry it passes, so a game
        // curated `none` but detected EAC is still gated.
        let mut entry = CompatEntry::from_toml(CYBERPUNK).unwrap();
        entry.anticheat = AntiCheat::BattlEye;
        let gate = InjectionGate::from_compat(Some(&entry), false);
        assert!(gate.protected() && gate.refuses());
        assert_eq!(gate.effective(), AntiCheat::BattlEye);
        // A recorded confirmation clears it.
        assert!(!InjectionGate::from_compat(Some(&entry), true).refuses());
        // And EAC Linux v2 is EAC for the gate's purposes.
        let eac_v2 = InjectionGate::new(AntiCheat::None, true, false);
        assert_eq!(eac_v2.effective(), AntiCheat::Eac);
        assert!(eac_v2.refuses());
    }

    fn pkg_arch_x86_64() -> PkgArch {
        PkgArch::X86_64
    }

    fn nr(pkg_arch: PkgArch) -> NeuralRendering {
        use pitagames_core::profile::{ModelSource, NrBackend, NrModel, NrProcessing, NrRoute};
        NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.8".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 100,
            processing: NrProcessing::RenoDx,
            pkg_arch,
            captured_env: Default::default(),
            model_source: ModelSource::CachedInPackage,
        }
    }

    // -- The mappings the compiler consumes ---------------------------------

    #[test]
    fn every_profile_graphics_choice_maps_to_a_field() {
        for upscaler in [
            Upscaler::Off,
            Upscaler::Native,
            Upscaler::SystemFsr,
            Upscaler::Fsr4Upgrade { version: None },
            Upscaler::Fsr4Rdna3 { version: None },
            Upscaler::DlssToFsr4,
            Upscaler::DlssToFsr31,
            Upscaler::DlssToXess,
            Upscaler::XessUpgrade,
            Upscaler::Ffx3Upgrade,
            Upscaler::Ffx4Upgrade,
            Upscaler::HlxDlss,
            Upscaler::HlxFsr4,
            Upscaler::HlxFsr4DropIn,
        ] {
            let field = Field::for_upscaler(&upscaler).expect("an upscaler always maps");
            assert!(
                Field::ALL.contains(&field),
                "{field:?} is not in Field::ALL"
            );
        }
        assert_eq!(Field::for_frame_gen(FrameGen::Off), None);
        assert_eq!(
            Field::for_frame_gen(FrameGen::Fsr4Mlfg),
            Some(Field::FrameGenFsr4Mlfg)
        );
        assert_eq!(
            Field::for_frame_gen(FrameGen::Fsr3),
            Some(Field::FrameGenFsr3)
        );
        assert_eq!(Field::for_injection(Injection::Auto), None);
        assert_eq!(
            Field::for_injection(Injection::EnvOnly),
            Some(Field::TierAInjection)
        );
        assert_eq!(
            Field::for_injection(Injection::DlssNr),
            Some(Field::NeuralRendering)
        );
    }

    #[test]
    fn the_matrix_walks_every_field_in_order() {
        let rows = matrix(
            &profile(),
            &gpu(GpuArch::Rdna4),
            Some(&cyberpunk()),
            Some(&cachyos("11.0-20260516")),
        );
        assert_eq!(rows.len(), Field::ALL.len());
        assert_eq!(rows[0].0, Field::Native);
        assert_eq!(rows[rows.len() - 1].0, Field::TierBInjection);
    }

    #[test]
    fn an_availability_round_trips_through_json_for_the_cli() {
        let json = serde_json::to_string(&Availability::hidden(reason::NR_OWNS_SLOT)).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"status":"hidden","reason":"{}"}}"#,
                reason::NR_OWNS_SLOT
            )
        );
        let parsed: Availability = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.reason(), Some(reason::NR_OWNS_SLOT));
        assert_eq!(
            serde_json::to_string(&Availability::available()).unwrap(),
            r#"{"status":"available"}"#
        );
    }

    // -- The docs table agrees with the UI ----------------------------------

    #[test]
    fn the_docs_table_quotes_every_reason_string() {
        // The UI's copy and the docs' "why" column must be the same strings, so
        // the document has to contain each one verbatim. A reason that is edited
        // here without editing the doc fails this test.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs/m3-capability.md");
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));

        for reason in REASONS {
            assert!(
                doc.contains(reason),
                "docs/m3-capability.md is missing the reason `{reason}`"
            );
        }
    }
}
