//! The NR uninstall: prefer upstream's own `remove`, then fall back to the
//! launcher's manifest, and prove the tree came back.
//!
//! Upstream's `install.sh` has a `remove` route (`bash install.sh <game folder>
//! remove`) that deletes every path its own `dlssnr-amd-install.txt` ledger
//! names. That is the first choice, because it is the vendor's own answer and it
//! knows files our manifest cannot. But it only *deletes*: it never puts back a
//! game file an install overwrote. Our own [`pitagames_mods::ModManifest`]
//! records that, with `.gl-backup/`, so it is the fallback — and the only path
//! that can promise "the game folder came back byte-for-byte".
//!
//! Three rules:
//!
//! - **Upstream first.** [`strategy`] prefers [`Strategy::UpstreamRemove`] when
//!   the package's installer and the game's ledger are both on disk; the manifest
//!   is used only when the upstream path is not available.
//! - **Check before touching.** [`restore_manifest`] verifies the installed tree
//!   against the manifest with BLAKE3 (per file and whole-tree) *before* it
//!   restores. A mismatch is reported with the files that changed and the restore
//!   does not proceed, because a restore over a tree that no longer matches what
//!   it recorded cannot be trusted.
//! - **Report the tree hash.** A completed restore reports whether the directory
//!   came back to the manifest's pre-install hash, so "byte-for-byte" is a value,
//!   not a claim.
//!
//! Framework-free and path-explicit: a `tempfile` tree and a fixture package
//! exercise the whole flow with no game, no GPU and no process.

use std::path::{Path, PathBuf};

use pitagames_mods::manifest::{ModError, ModIssue, ModManifest, tree_hash};

use crate::nr_install::{INSTALLER_PROGRAM, INSTALLER_SCRIPT};

/// The `install.sh` route that uninstalls.
pub const REMOVE_ROUTE: &str = "remove";

/// The ledger upstream's installer writes into the game directory and its
/// `remove` route deletes by.
pub const UPSTREAM_LEDGER: &str = "dlssnr-amd-install.txt";

/// The upstream remove command: `bash <package>/install.sh <game dir> remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovePlan {
    /// The program to run (`bash`).
    pub program: String,
    /// The argv, never a shell string.
    pub args: Vec<String>,
    /// The game's executable directory, where upstream's ledger lives.
    pub cwd: PathBuf,
}

/// Why the launcher will not compose an upstream remove.
#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error("{path} does not contain the DLSSNR installer script ({script})")]
    MissingInstaller { path: PathBuf, script: &'static str },

    #[error("no game directory at {}", path.display())]
    MissingGameDir { path: PathBuf },
}

/// Compose upstream's remove command for a game directory.
///
/// Refuses when the package has no `install.sh` (nothing to run) or the game
/// directory is gone, naming which.
pub fn remove_plan(package_dir: &Path, game_dir: &Path) -> Result<RemovePlan, RemoveError> {
    let script = package_dir.join(INSTALLER_SCRIPT);
    if !script.is_file() {
        return Err(RemoveError::MissingInstaller {
            path: package_dir.to_path_buf(),
            script: INSTALLER_SCRIPT,
        });
    }
    if !game_dir.is_dir() {
        return Err(RemoveError::MissingGameDir {
            path: game_dir.to_path_buf(),
        });
    }
    Ok(RemovePlan {
        program: INSTALLER_PROGRAM.to_string(),
        args: vec![
            script.to_string_lossy().into_owned(),
            game_dir.to_string_lossy().into_owned(),
            REMOVE_ROUTE.to_string(),
        ],
        cwd: game_dir.to_path_buf(),
    })
}

/// Whether the package carries upstream's installer, so its `remove` can run.
pub fn upstream_available(package_dir: &Path) -> bool {
    package_dir.join(INSTALLER_SCRIPT).is_file()
}

/// Whether the game directory holds upstream's uninstall ledger.
pub fn has_upstream_ledger(game_dir: &Path) -> bool {
    game_dir.join(UPSTREAM_LEDGER).is_file()
}

/// Which mechanism an uninstall uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Upstream's own `remove` runs: its installer and ledger are both present.
    UpstreamRemove,
    /// The launcher's `.gl-backup` manifest restore is the way back.
    Manifest,
    /// Neither is present; there is nothing to uninstall.
    Nothing,
}

/// Choose the uninstall path for a game.
///
/// Upstream wins when it can actually remove something (its installer *and* its
/// ledger exist); otherwise the launcher's manifest; otherwise nothing.
pub fn strategy(package_dir: &Path, game_dir: &Path) -> Strategy {
    if upstream_available(package_dir) && has_upstream_ledger(game_dir) {
        Strategy::UpstreamRemove
    } else if ModManifest::is_installed(game_dir) {
        Strategy::Manifest
    } else {
        Strategy::Nothing
    }
}

/// What a manifest fallback uninstall found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallReport {
    /// The mod the manifest named.
    pub mod_name: String,
    /// Whether the restore ran. `false` when the pre-flight check refused.
    pub restored: bool,
    /// The BLAKE3 tree hash after the operation.
    pub tree_hash: String,
    /// The pre-install hash the manifest recorded.
    pub expected_tree_hash: String,
    /// Whether `tree_hash` equals `expected_tree_hash`.
    pub tree_hash_ok: bool,
    /// Files the mod created that were deleted.
    pub removed: Vec<String>,
    /// Originals moved back out of `.gl-backup/`.
    pub files_restored: Vec<String>,
    /// What changed, when the operation did not cleanly restore.
    pub issues: Vec<ModIssue>,
}

impl UninstallReport {
    /// Whether the tree was restored and came back byte-for-byte.
    pub fn ok(&self) -> bool {
        self.restored && self.tree_hash_ok
    }

    /// One line for a status panel.
    pub fn summary(&self) -> String {
        if !self.restored {
            return format!(
                "Refused to uninstall {}: the game tree no longer matches its manifest ({} change(s)). \
                 Nothing was touched.",
                self.mod_name,
                self.issues.len()
            );
        }
        if self.tree_hash_ok {
            format!(
                "Removed {} ({} file(s) deleted, {} restored); the tree matches its pre-install hash.",
                self.mod_name,
                self.removed.len(),
                self.files_restored.len()
            )
        } else {
            format!(
                "Removed {} ({} file(s)); the tree hash does not match the pre-install one.",
                self.mod_name,
                self.removed.len()
            )
        }
    }
}

/// Restore a game directory from its manifest, refusing to proceed on a mismatch.
///
/// The manifest's per-file and whole-tree BLAKE3 hashes are checked first: when
/// the installed tree no longer matches what the manifest recorded, the report
/// carries the changed files and the restore does **not** run. On a clean match
/// the restore runs and the report says whether the directory reached the
/// manifest's pre-install hash.
pub fn restore_manifest(root: &Path) -> Result<UninstallReport, ModError> {
    let manifest = ModManifest::load_required(root)?;

    let check = manifest.verify(root);
    if !check.ok() {
        return Ok(UninstallReport {
            mod_name: manifest.mod_name.clone(),
            restored: false,
            tree_hash: tree_hash(root)?,
            expected_tree_hash: manifest.pre_install_tree_hash.clone(),
            tree_hash_ok: false,
            removed: Vec::new(),
            files_restored: Vec::new(),
            issues: check.issues,
        });
    }

    let report = ModManifest::restore(root)?;
    Ok(UninstallReport {
        mod_name: report.mod_name,
        restored: true,
        tree_hash: report.tree_hash,
        expected_tree_hash: report.expected_tree_hash,
        tree_hash_ok: report.tree_hash_ok,
        removed: report.removed,
        files_restored: report.restored,
        issues: report.issues,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use pitagames_mods::manifest::{ModManifest, ModSource};

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A game directory with one ordinary file and one a mod will replace.
    fn game(root: &Path) {
        write(&root.join("game.exe"), b"the game");
        write(&root.join("dxgi.dll"), b"the game's own dxgi");
    }

    fn game_dir(dir: &Path) -> PathBuf {
        let root = dir.join("game");
        fs::create_dir_all(&root).unwrap();
        game(&root);
        root
    }

    /// Two files a mod installs: one replaces an existing game file, one is new.
    fn nr_sources(staging: &Path) -> Vec<ModSource> {
        let dll = staging.join("OptiScaler.dll");
        let ini = staging.join("dlssnr-amd.ini");
        write(&dll, b"the new dll");
        write(&ini, b"[Network]\nModel = CC_SMALL_FP8\n");
        vec![
            ModSource::new("dxgi.dll", dll),
            ModSource::new("dlssnr-amd.ini", ini),
        ]
    }

    fn installed(dir: &Path) -> (PathBuf, String) {
        let root = game_dir(dir);
        let before = tree_hash(&root).unwrap();
        ModManifest::install(&root, "dlssnr", &nr_sources(&dir.join("staging"))).unwrap();
        (root, before)
    }

    /// A fixture package directory holding upstream's `install.sh`.
    fn package(dir: &Path) -> PathBuf {
        let package = dir.join("pkg");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join(INSTALLER_SCRIPT), "#!/bin/sh\n").unwrap();
        package
    }

    // -- the upstream remove plan -------------------------------------------

    #[test]
    fn the_remove_plan_composes_upstream_remove_in_the_game_dir() {
        let dir = tempfile::tempdir().unwrap();
        let package = package(dir.path());
        let game = game_dir(dir.path());

        let plan = remove_plan(&package, &game).unwrap();

        assert_eq!(plan.program, INSTALLER_PROGRAM);
        assert_eq!(
            plan.args[0],
            package.join(INSTALLER_SCRIPT).to_string_lossy()
        );
        assert_eq!(plan.args[1], game.to_string_lossy());
        assert_eq!(plan.args[2], REMOVE_ROUTE);
        assert_eq!(plan.cwd, game);
    }

    #[test]
    fn a_package_without_the_installer_has_no_remove() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let game = game_dir(dir.path());

        let error = remove_plan(&empty, &game).unwrap_err();
        assert!(matches!(error, RemoveError::MissingInstaller { .. }));
        assert!(error.to_string().contains(INSTALLER_SCRIPT));
        assert!(!upstream_available(&empty));
    }

    #[test]
    fn a_missing_game_directory_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let package = package(dir.path());
        let error = remove_plan(&package, &dir.path().join("gone")).unwrap_err();
        assert!(matches!(error, RemoveError::MissingGameDir { .. }));
    }

    // -- the strategy -------------------------------------------------------

    #[test]
    fn upstream_remove_is_preferred_and_the_manifest_is_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let package = package(dir.path());
        let (root, _before) = installed(dir.path());

        // A launcher manifest alone: the fallback.
        assert!(ModManifest::is_installed(&root));
        assert!(!has_upstream_ledger(&root));
        assert_eq!(strategy(&package, &root), Strategy::Manifest);

        // Upstream's ledger appears: upstream wins.
        write(&root.join(UPSTREAM_LEDGER), b"dxgi.dll\n");
        assert_eq!(strategy(&package, &root), Strategy::UpstreamRemove);

        // No package installer: the manifest is used even with a ledger.
        let empty = dir.path().join("no-package");
        fs::create_dir_all(&empty).unwrap();
        assert!(!upstream_available(&empty));
        assert_eq!(strategy(&empty, &root), Strategy::Manifest);

        // Neither a ledger nor a manifest: nothing to do.
        let bare = game_dir(&dir.path().join("bare"));
        assert_eq!(strategy(&package, &bare), Strategy::Nothing);
    }

    // -- the manifest fallback ----------------------------------------------

    #[test]
    fn uninstall_over_a_fixture_tree_restores_the_pre_install_hash() {
        let dir = tempfile::tempdir().unwrap();
        let (root, before) = installed(dir.path());
        assert_ne!(tree_hash(&root).unwrap(), before);

        let report = restore_manifest(&root).unwrap();

        assert!(report.restored, "{report:?}");
        assert!(report.tree_hash_ok, "{report:?}");
        assert!(report.ok());
        assert_eq!(report.tree_hash, before);
        assert_eq!(report.expected_tree_hash, before);
        assert_eq!(report.mod_name, "dlssnr");
        assert_eq!(report.removed, ["dlssnr-amd.ini", "dxgi.dll"]);
        assert_eq!(report.files_restored, ["dxgi.dll"]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);

        // The tree is byte-for-byte the original and the ledger is gone.
        assert_eq!(tree_hash(&root).unwrap(), before);
        assert_eq!(
            fs::read(root.join("dxgi.dll")).unwrap(),
            b"the game's own dxgi"
        );
        assert!(!root.join("dlssnr-amd.ini").exists());
        assert!(!ModManifest::is_installed(&root));
        assert!(report.summary().contains("pre-install hash"));
    }

    #[test]
    fn a_modified_created_file_is_reported_and_nothing_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let (root, _before) = installed(dir.path());
        write(&root.join("dxgi.dll"), b"the user changed this");
        let modified = tree_hash(&root).unwrap();

        let report = restore_manifest(&root).unwrap();

        assert!(
            !report.restored,
            "it must refuse rather than proceed: {report:?}"
        );
        assert!(!report.tree_hash_ok);
        assert!(!report.ok());
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                ModIssue::ModifiedCreated { path, .. } if path == "dxgi.dll"
            )),
            "the changed file is named: {:?}",
            report.issues
        );
        assert!(report.summary().contains("Refused"));

        // Nothing was moved: the modified file and the ledger are as they were.
        assert_eq!(tree_hash(&root).unwrap(), modified);
        assert!(ModManifest::is_installed(&root));
        assert_eq!(
            fs::read(root.join("dxgi.dll")).unwrap(),
            b"the user changed this"
        );
    }

    #[test]
    fn an_unrecorded_file_is_a_tree_hash_mismatch_reported_with_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let (root, _before) = installed(dir.path());
        write(&root.join("added-later.dll"), b"nobody recorded this");

        let report = restore_manifest(&root).unwrap();

        assert!(!report.restored, "{report:?}");
        let mismatch = report
            .issues
            .iter()
            .find_map(|issue| match issue {
                ModIssue::TreeHashMismatch { expected, actual } => Some((expected, actual)),
                _ => None,
            })
            .expect("a whole-tree mismatch is reported");
        assert_eq!(
            mismatch.0,
            &ModManifest::load_required(&root)
                .unwrap()
                .post_install_tree_hash
        );
        assert_ne!(mismatch.1, mismatch.0);
        // The unrecorded file was left alone.
        assert_eq!(
            fs::read(root.join("added-later.dll")).unwrap(),
            b"nobody recorded this"
        );
    }

    #[test]
    fn restore_without_a_manifest_reports_a_missing_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let root = game_dir(dir.path());
        let error = restore_manifest(&root).unwrap_err();
        assert!(matches!(error, ModError::NoManifest { .. }), "{error}");
    }
}
