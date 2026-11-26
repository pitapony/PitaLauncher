//! Mod install manifests for PitaGames.
//!
//! Framework-free by design, like `pitagames-platform`: the GUI and the CLI
//! call in and render what they report, and every rule is testable against a
//! `tempfile` tree with no game, no GPU and no webview.
//!
//! M3, M3.5 and M4 deploy three different mod systems — Tier B OptiScaler,
//! HelixSR and DLSSNR-AMD — and each one is a set of files dropped into a game
//! directory over whatever was already there. `manifest` is the one place that
//! remembers what a mod did, so "uninstall restores byte-for-byte" is a single
//! mechanism instead of three: every file it created, every file it moved into
//! `.gl-backup/`, a BLAKE3 hash per file, a pre-install tree hash, and the
//! verified restore that proves the tree came back.

pub mod manifest;

pub use manifest::{
    BACKUP_DIR, MANIFEST_NAME, MANIFEST_VERSION, ModEntry, ModError, ModIssue, ModManifest,
    ModSource, RestoreReport, VerifyReport, file_hash, tree_hash,
};
