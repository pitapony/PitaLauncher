//! Domain types shared by every PitaGames crate.
//!
//! This crate deliberately has no UI framework dependencies and almost no I/O:
//! it is the boundary that keeps the Dioxus renderer replaceable. The exceptions
//! are [`profile_file`] — the launcher's own pre-compiled shim profile, which the
//! GUI, the CLI and the shim must all agree on byte for byte — and the two
//! desktop-integration files the launcher owns: the generated
//! [`desktop`] entry and the [`activation`] socket a second launch talks to.

pub mod a11y;
#[cfg(unix)]
pub mod activation;
pub mod design;
pub mod desktop;
pub mod i18n;
pub mod launch;
pub mod lock;
pub mod metadata;
pub mod paths;
pub mod perf;
pub mod profile;
pub mod profile_file;
pub mod release;
pub mod session;
pub mod store;
pub mod ui;

#[cfg(unix)]
pub use activation::{Activation, ActivationError, ActivationServer, request_focus};
pub use desktop::{
    APP_ID, DESKTOP_SPEC_VERSION, DesktopEntry, DesktopEntryError, ENTRY_FILE_NAME, InstallOutcome,
};
pub use i18n::{Catalog, CatalogError, Language, t};
pub use launch::{
    LaunchSpec, SHIM_PROGRAM, TokenizeError, flatten_wrappers, render_command,
    render_shell_command, shell_quote, shim_launch_options, steam_install, steam_run,
    strip_shim_launch_options, tokenize_command,
};
pub use lock::{InstanceLock, LockError};
pub use metadata::{AppKind, GameMetadata, Platforms};
pub use perf::{
    BURST_GAP_MS, Budget, COLD_START_BUDGET_MS, FLUSH_INTERVAL_MS, FRAME_TIME_BUDGET_MS,
    FrameStats, LIBRARY_GAMES, MAX_FRAME_SAMPLES, MEMORY_CEILING_BYTES, Measurement, Metric,
    PerfError, PerfRecorder, Verdict, Violation,
};
pub use profile::{
    Diagnostics, FrameGen, GamescopeOptions, Injection, InjectionDll, ModelSource, NeuralRendering,
    NrBackend, NrModel, NrProcessing, NrRoute, PkgArch, Profile, ProfileError, ProtonRef,
    SlotOwner, Upscaler, WrapperChain, WrapperStep,
};
pub use profile_file::{
    ProfileFileError, SHIM_PROFILE_SCHEMA, ShimProfile, ShimProton, ShimProtonInstall,
    load_shim_profile, read_shim_profile, remove_shim_profile, save_shim_profile,
    shim_profile_path,
};
pub use release::{
    DEV_PACKAGE, INSTALL_PATHS, PACKAGE, SHA256_PLACEHOLDER, TAG_PREFIX, tag as release_tag,
};
pub use session::{
    CONSOLE_FLAG, GAMESCOPE_PROGRAM, LAUNCHER_PROGRAM, SESSION_ENV, SESSION_FLAG, SessionEntry,
    SessionError, SystemdUnit, UNIT_NAME, gamescope_arguments, launcher_argv,
    packaged_entry as packaged_session_entry, packaged_unit as packaged_session_unit, session_exec,
};
pub use store::{AntiCheat, Game, GameId, OwnedGame, Store, UnknownAntiCheat, UnknownStore};
pub use ui::{
    AppConfig, CompatConfig, ConfigError, DisplayConfig, SessionDisplay, SessionResolution, Shell,
    StartMode, UiConfig, UnknownShell, UnknownStartMode,
};
