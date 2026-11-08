//! Host platform detection for PitaGames.
//!
//! Framework-free by design: the GUI renders what this crate reports and never
//! does detection itself, which keeps the renderer replaceable and the
//! detection testable against fixture directories.
//!
//! M1 ships [`proton`] discovery and the runtime subset of [`envc`]. GPU
//! detection, the graphics env table and process supervision arrive in M3.

pub mod envc;
pub mod proton;

pub use envc::{CompileRequest, CompiledRuntime, EnvcError, compile_runtime, shim_proton};
pub use proton::{
    DiscoveryReport, ProtonError, ProtonInstall, ProtonKind, ProtonRoot, ProtonSource,
    default_roots, discover, discover_in,
};
