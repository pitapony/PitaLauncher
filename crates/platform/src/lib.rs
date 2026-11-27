//! Host platform detection for PitaGames.
//!
//! Framework-free by design: the GUI renders what this crate reports and never
//! does detection itself, which keeps the renderer replaceable and the
//! detection testable against fixture directories.
//!
//! M1 ships [`proton`] discovery, the runtime subset of [`envc`], the
//! [`compat`] database loader, [`anticheat`] detection with the injection gate,
//! and the read-only [`logs`] reader. M2 adds [`disk`] (free space and bounded
//! directory sizes), [`prefix`] (the per-game Wine/Proton prefix layout for the
//! non-Steam stores) and [`launch`] (the umu/Proton hand-off plus process
//! supervision). M3 begins with [`gpu`], the injected GPU probe (sysfs, the
//! `const` vendor/device table, `vulkaninfo`, VRAM and the ntsync/HDR hints)
//! that the capability matrix will build on. [`capability`] is that matrix: it
//! folds detection, curation and Proton resolution into one `Available` /
//! `Hidden(reason)` verdict and one reason string per field. M4 adds
//! [`proton_update`]: opt-in, pinned GE-Proton builds fetched through
//! `mods::release` into our own `proton/` directory, which [`proton`]
//! discovery already scans.

pub mod anticheat;
pub mod capability;
pub mod compat;
pub mod disk;
pub mod eac;
pub mod envc;
pub mod gpu;
pub mod launch;
pub mod logs;
pub mod nr;
pub mod prefix;
pub mod proton;
pub mod proton_update;
pub mod tier_a;
pub mod validation;

pub use anticheat::{
    AnticheatFinding, AnticheatScan, AnticheatSource, EffectiveAnticheat, GateDecision, ScanLimits,
    detect, detect_limited, effective, gate,
};
pub use capability::{Availability, Field, REASONS, availability, matrix, reason};
pub use compat::{
    Arch, CompatDb, CompatEntry, CompatError, CompatWarning, GameApi, HelixsrMode, HelixsrPlan,
    NeuralRules, NrBackendChoice, PreferredDll, RuntimeRules, UpscalerId, Version, WrapperId,
    default_shipped_dirs, default_user_dirs,
};
pub use disk::{DirSize, SizeLimits, Space, SpaceCheck};
pub use eac::{
    EacProbe, EacReadiness, EacVerdict, NOT_VALIDATED, Requirement, RequirementCheck, probe,
    reboot_argv, verdict,
};
pub use envc::{
    CompileRequest, CompiledRuntime, EnvcError, GraphicsContext, compile, compile_runtime,
    shim_proton,
};
// `detect` stays namespaced (`gpu::detect`) so it cannot collide with
// `anticheat::detect`.
pub use gpu::{GpuArch, GpuProbe, GpuProfile, GpuVendor, HdrSupport, arch_for};
pub use launch::{
    DEFAULT_GRACE, Handoff, LaunchError, LaunchMode, Outcome, Prepared, RunOptions, Session,
    UMU_RUN, prepare, start, terminate,
};
pub use logs::{LogFile, LogKind, TAIL_BYTES, Tail};
pub use nr::{
    BackendPin, BackendRegistry, Cost, NrError, NrPlan, NrRequest, NrVerdict, Resolution,
    backend_for, default_scale, is_experimental, model_scale_preset, pe_is_64_bit,
};
pub use prefix::{PREFIX_DIR_ENV, Prefix};
pub use proton::{
    DiscoveryReport, ProtonError, ProtonInstall, ProtonKind, ProtonRoot, ProtonSource,
    default_roots, discover, discover_in,
};
pub use proton_update::{
    BuildState, ProtonPin, ProtonUpdateError, Registry, RegistryError, UpdateReport,
};
pub use tier_a::{TierAGate, dll_name, env as tier_a_env, gate as tier_a_gate};
pub use validation::{Claim, Machine, ValidationPath};
