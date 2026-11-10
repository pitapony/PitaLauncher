//! Domain types shared by every PitaGames crate.
//!
//! This crate deliberately has no UI framework dependencies and almost no I/O:
//! it is the boundary that keeps the Dioxus renderer replaceable. The one
//! exception is [`profile_file`] — the launcher's own pre-compiled shim profile,
//! which the GUI, the CLI and the shim must all agree on byte for byte.

pub mod launch;
pub mod lock;
pub mod metadata;
pub mod paths;
pub mod profile;
pub mod profile_file;
pub mod store;

pub use launch::{
    LaunchSpec, SHIM_PROGRAM, TokenizeError, flatten_wrappers, render_command, shell_quote,
    shim_launch_options, steam_install, steam_run, strip_shim_launch_options, tokenize_command,
};
pub use lock::{InstanceLock, LockError};
pub use metadata::{AppKind, GameMetadata, Platforms};
pub use profile::{
    Diagnostics, FrameGen, GamescopeOptions, Injection, InjectionDll, ModelSource, NeuralRendering,
    NrBackend, NrModel, NrProcessing, NrRoute, PkgArch, Profile, ProfileError, ProtonRef, Upscaler,
    WrapperChain, WrapperStep,
};
pub use profile_file::{
    ProfileFileError, SHIM_PROFILE_SCHEMA, ShimProfile, ShimProton, ShimProtonInstall,
    load_shim_profile, read_shim_profile, remove_shim_profile, save_shim_profile,
    shim_profile_path,
};
pub use store::{AntiCheat, Game, GameId, OwnedGame, Store, UnknownAntiCheat, UnknownStore};
