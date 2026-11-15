//! Store backends.
//!
//! The v1 design wraps external CLIs (legendary, nile, gogdl) behind a
//! [`StoreBackend`] trait; Steam is read natively from VDF/ACF manifests and
//! `localconfig.vdf`, and its per-game config is written format-preservingly
//! (`localconfig.vdf` launch options, `config.vdf` compatibility-tool mappings).
//! [`cli`] is the PTY driver those stores share, [`report`] is how a finished
//! run is judged (both CLIs log an error and exit `0`), [`status`] is the store
//! status row and the unified update summary, and [`epic`], [`amazon`] and
//! [`gog`] are the backends on top of it (`legendary`: auth, list, install,
//! verify, launch; `nile`: the same plus a library sync; GOG: `lgogdownloader`
//! for the library/login and `gogdl` for install/repair/launch). [`queue`] is the
//! serial install queue those backends share, with the disk-space pre-check,
//! [`sync`] folds the two listings each backend can answer into one round of
//! `Game` rows and merges them over the cache without deleting anything a failed
//! listing could not vouch for, and [`installer`] turns a queued job into the
//! backend run that fills its directory.

use std::path::PathBuf;

pub mod acf;
pub mod amazon;
pub mod cli;
pub mod compat_tool;
pub mod epic;
pub mod gog;
pub mod import;
pub mod install;
pub mod installer;
pub mod localconfig;
pub mod queue;
pub mod report;
pub mod status;
pub mod steam;
pub mod steam_config;
pub mod sync;
pub mod vdf;
mod vdf_edit;
pub mod watch;

pub use acf::{AppManifest, DownloadProgress, InstallPhase, ManifestError, flags};
pub use amazon::{Amazon, AmazonAccount, AmazonError, VerifyReport as AmazonVerifyReport};
pub use cli::{
    CliCommand, CliError, CliHandle, CliOptions, CliOutcome, CliSpec, ProgressEvent,
    ProgressParser, StoreStatus, TimeoutKind, resolve_program,
};
pub use compat_tool::{CompatTool, DEFAULT_PRIORITY};
pub use epic::{Epic, EpicAccount, EpicError};
pub use gog::{Gog, GogAccount, GogError};
pub use import::{ImportReport, Importer, Origin, SourceReport};
pub use install::{DEFAULT_POLL_INTERVAL, InstallMonitor, InstallProgress, InstallSample};
pub use installer::{InstallPlan, InstallRun, InstallerError};
pub use localconfig::{Edit, LAUNCH_OPTIONS};
pub use queue::{InstallJob, InstallQueue, JobProgress, JobState, QueueError, StartOutcome};
pub use report::{LogFormat, LogLine, RunFailure, RunReport};
pub use status::{
    CLI_STORES, CliTool, RowState, SessionState, StoreStatusReport, UPDATE_TAG, UpdateAvailable,
    UpdateSource, stores, updates,
};
pub use steam::{
    LibraryList, ScanReport, discover_libraries, parse_library_folders, scan_steam, user_configs,
};
pub use steam_config::{
    CompatRestoreReport, CompatWriteReport, RestoreReport, SteamProcessState, WriteOptions,
    WriteReport, apply_compat_tool, apply_launch_options, detect_steam_process, read_compat_tool,
    read_launch_options, restore_compat_tool, restore_launch_options,
};
pub use sync::{Backends, StoreLibrary, SyncReport};
pub use watch::{ChangeOp, DEFAULT_DEBOUNCE, Debouncer, LibraryChange, LibraryWatcher, WatchError};

/// One lock for every test that mutates the process environment.
///
/// `std::env` is process-wide, and the crate's tests run in parallel, so the
/// XDG and store-CLI variables are serialized across modules instead of per
/// module (two module-local locks still race each other).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Errors that cross the store boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {source}")]
    Manifest {
        path: PathBuf,
        #[source]
        source: ManifestError,
    },
}
