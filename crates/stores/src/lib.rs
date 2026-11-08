//! Store backends.
//!
//! The v1 design wraps external CLIs (legendary, nile, gogdl) behind a
//! [`StoreBackend`] trait; Steam is read natively from VDF/ACF manifests and
//! `localconfig.vdf`, and its per-game config is written format-preservingly
//! (`localconfig.vdf` launch options, `config.vdf` compatibility-tool mappings).
//! Only the Steam backend is implemented so far.

use std::path::PathBuf;

pub mod acf;
pub mod compat_tool;
pub mod install;
pub mod localconfig;
pub mod steam;
pub mod steam_config;
pub mod vdf;
mod vdf_edit;
pub mod watch;

pub use acf::{AppManifest, DownloadProgress, InstallPhase, ManifestError, flags};
pub use compat_tool::{CompatTool, DEFAULT_PRIORITY};
pub use install::{DEFAULT_POLL_INTERVAL, InstallMonitor, InstallProgress, InstallSample};
pub use localconfig::{Edit, LAUNCH_OPTIONS};
pub use steam::{
    LibraryList, ScanReport, discover_libraries, parse_library_folders, scan_steam, user_configs,
};
pub use steam_config::{
    CompatRestoreReport, CompatWriteReport, RestoreReport, SteamProcessState, WriteOptions,
    WriteReport, apply_compat_tool, apply_launch_options, detect_steam_process, read_compat_tool,
    read_launch_options, restore_compat_tool, restore_launch_options,
};
pub use watch::{ChangeOp, DEFAULT_DEBOUNCE, Debouncer, LibraryChange, LibraryWatcher, WatchError};

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
