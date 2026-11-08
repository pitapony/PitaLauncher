//! Domain types shared by every PitaGames crate.
//!
//! This crate deliberately has no I/O and no UI framework dependencies: it is
//! the boundary that keeps the Dioxus renderer replaceable.

pub mod launch;
pub mod metadata;
pub mod paths;
pub mod profile;
pub mod store;

pub use launch::{
    LaunchSpec, SHIM_PROGRAM, TokenizeError, shim_launch_options, steam_install, steam_run,
    strip_shim_launch_options, tokenize_command,
};
pub use metadata::{AppKind, GameMetadata, Platforms};
pub use profile::{
    Diagnostics, FrameGen, GamescopeOptions, Injection, InjectionDll, ModelSource, NeuralRendering,
    NrBackend, NrModel, NrProcessing, NrRoute, PkgArch, Profile, ProfileError, ProtonRef, Upscaler,
    WrapperChain, WrapperStep,
};
pub use store::{AntiCheat, Game, GameId, Store, UnknownAntiCheat, UnknownStore};
