//! The mod half of a repair: find every manifest, verify it, re-apply it.
//!
//! A mod drops files into a game directory and records them in a
//! [`ModManifest`](pitagames_mods::ModManifest). Steam's "verify integrity" and
//! a stray editor can quietly change or remove those files, which no scan sees
//! unless someone re-hashes the tree. [`check_roots`] does exactly that: it
//! walks the roots, verifies every manifest it finds, and reports the ones that
//! no longer match. A tampered tree is not a dead end — [`reapply`] restores the
//! tree to its pre-install state and installs the mod again from the caller's
//! sources, so the recorded hashes come back.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use pitagames_mods::{BACKUP_DIR, ModError, ModIssue, ModManifest, ModSource, RestoreReport};
use serde::Serialize;
use walkdir::WalkDir;

/// How deep a scan looks for a game directory holding a mod manifest.
///
/// Mods land beside an executable, which for an Unreal game is
/// `.../Binaries/Win64`; eight levels covers every layout PitaGames deploys
/// into without walking a whole Steam library forever.
pub const SCAN_DEPTH: usize = 8;

/// How a found manifest compares to the tree it describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ModState {
    /// Every created file and every backup matches the manifest.
    Clean,
    /// The tree no longer matches the manifest.
    Tampered {
        /// Each mismatch, in path order.
        issues: Vec<ModIssue>,
    },
    /// The manifest itself could not be read.
    Unreadable {
        /// Why it could not be read.
        message: String,
    },
}

/// One game directory that holds a mod manifest, and its verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModFinding {
    /// The game directory the manifest lives in.
    pub dir: PathBuf,
    /// The mod's name, when the manifest was readable.
    pub mod_name: Option<String>,
    /// What the check found.
    pub state: ModState,
}

impl ModFinding {
    /// Whether the tree matches its manifest.
    #[must_use]
    pub fn ok(&self) -> bool {
        matches!(self.state, ModState::Clean)
    }

    /// Whether the tree no longer matches, so a re-apply is offered.
    #[must_use]
    pub fn needs_reapply(&self) -> bool {
        matches!(self.state, ModState::Tampered { .. })
    }
}

/// Every game directory under `root` that holds a mod manifest.
///
/// The walk never descends into a `.gl-backup/` directory: a manifest is a
/// property of the game directory above it, and a backup tree can itself hold
/// files that resemble one. A missing root is an empty list, not an error.
#[must_use]
pub fn find_manifests(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }

    let mut found = Vec::new();
    let walk = WalkDir::new(root)
        .max_depth(SCAN_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(BACKUP_DIR));
    for entry in walk.flatten() {
        if entry.file_type().is_dir() && ModManifest::is_installed(entry.path()) {
            found.push(entry.path().to_path_buf());
        }
    }
    found.sort();
    found
}

/// Verify the manifest in one game directory.
///
/// A directory with no manifest, or one that cannot be read, is reported as
/// [`ModState::Unreadable`] rather than skipped: a repair that silently ignores
/// a broken ledger would be worse than one that names it.
#[must_use]
pub fn check_dir(dir: &Path) -> ModFinding {
    let manifest = match ModManifest::load(dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => {
            return ModFinding {
                dir: dir.to_path_buf(),
                mod_name: None,
                state: ModState::Unreadable {
                    message: format!("no mod manifest in {}", dir.display()),
                },
            };
        }
        Err(error) => {
            return ModFinding {
                dir: dir.to_path_buf(),
                mod_name: None,
                state: ModState::Unreadable {
                    message: error.to_string(),
                },
            };
        }
    };

    let report = manifest.verify(dir);
    let state = if report.ok() {
        ModState::Clean
    } else {
        ModState::Tampered {
            issues: report.issues,
        }
    };
    ModFinding {
        dir: dir.to_path_buf(),
        mod_name: Some(manifest.mod_name),
        state,
    }
}

/// Verify every manifest under every root, in directory order.
///
/// A directory named by two roots is checked once.
#[must_use]
pub fn check_roots(roots: &[PathBuf]) -> Vec<ModFinding> {
    let mut findings = Vec::new();
    for root in roots {
        for dir in find_manifests(root) {
            findings.push(check_dir(&dir));
        }
    }
    findings.sort_by(|a, b| a.dir.cmp(&b.dir));
    findings.dedup_by(|a, b| a.dir == b.dir);
    findings
}

/// What a re-apply did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReapplyReport {
    /// The tree restored to its pre-install state on the way through.
    pub restored: RestoreReport,
    /// The manifest the fresh install wrote.
    pub manifest: ModManifest,
}

/// Re-apply a mod over a tampered tree.
///
/// The tree is first restored to its pre-install state — which also removes the
/// old ledger, so the install is not refused as a second deployment — and then
/// the mod is installed again from `sources`. The result verifies clean against
/// the manifest it writes, so "detected and re-appliable" is one call rather
/// than a sequence a caller has to get right.
pub fn reapply(
    dir: &Path,
    mod_name: &str,
    sources: &[ModSource],
) -> Result<ReapplyReport, ModError> {
    let restored = ModManifest::restore(dir)?;
    let manifest = ModManifest::install(dir, mod_name, sources)?;
    Ok(ReapplyReport { restored, manifest })
}

/// Restore a modded tree to its pre-install state, uninstalling the mod.
///
/// The safe half of a repair: it needs only the manifest and its backups, and
/// it refuses rather than clobbers when a backup was tampered with.
pub fn restore(dir: &Path) -> Result<RestoreReport, ModError> {
    ModManifest::restore(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const MOD_NAME: &str = "optiscaler";

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A game tree with one file a mod will replace and one it will add.
    fn game(root: &Path) {
        write(&root.join("game.exe"), b"the game");
        write(&root.join("dxgi.dll"), b"the game's own dxgi");
    }

    /// The two files a fixture mod installs, staged outside the game tree: one
    /// replaces a file the game owns (so a backup exists), one is new.
    fn sources(staging: &Path) -> Vec<ModSource> {
        let dll = staging.join("OptiScaler.dll");
        let ini = staging.join("OptiScaler.ini");
        write(&dll, b"the mod dll");
        write(&ini, b"[OptiScaler]\nEnabled=true\n");
        vec![
            ModSource::new("dxgi.dll", dll),
            ModSource::new("OptiScaler.ini", ini),
        ]
    }

    #[test]
    fn a_clean_install_checks_clean() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        game(&root);
        ModManifest::install(&root, MOD_NAME, &sources(&dir.path().join("staging"))).unwrap();

        let finding = check_dir(&root);
        assert!(finding.ok(), "{finding:?}");
        assert_eq!(finding.mod_name.as_deref(), Some(MOD_NAME));
    }

    #[test]
    fn a_tampered_tree_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        game(&root);
        ModManifest::install(&root, MOD_NAME, &sources(&dir.path().join("staging"))).unwrap();

        // Steam verifies integrity and quietly drops the original back.
        write(&root.join("dxgi.dll"), b"the game's own dxgi");

        let finding = check_dir(&root);
        assert!(finding.needs_reapply(), "{finding:?}");
        let ModState::Tampered { issues } = &finding.state else {
            panic!("{finding:?}");
        };
        assert!(
            issues.iter().any(|issue| matches!(
                issue,
                ModIssue::ModifiedCreated { path, .. } if path == "dxgi.dll"
            )),
            "{issues:?}"
        );
    }

    #[test]
    fn a_tampered_tree_is_re_appliable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        game(&root);
        let staging = dir.path().join("staging");
        let first = ModManifest::install(&root, MOD_NAME, &sources(&staging)).unwrap();
        let clean_hash = first.post_install_tree_hash.clone();

        write(&root.join("dxgi.dll"), b"reverted");
        assert!(check_dir(&root).needs_reapply());

        let report = reapply(&root, MOD_NAME, &sources(&staging)).unwrap();
        assert_eq!(report.manifest.post_install_tree_hash, clean_hash);
        assert_eq!(
            fs::read(root.join("dxgi.dll")).unwrap(),
            b"the mod dll",
            "the re-apply put the mod's bytes back"
        );

        let finding = check_dir(&root);
        assert!(finding.ok(), "{finding:?}");
    }

    #[test]
    fn re_apply_refuses_a_tampered_backup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        game(&root);
        ModManifest::install(&root, MOD_NAME, &sources(&dir.path().join("staging"))).unwrap();

        // The backup of the displaced dxgi.dll was damaged: re-applying must not
        // proceed, because restoring would put the wrong bytes back.
        write(&root.join(BACKUP_DIR).join("dxgi.dll"), b"not the original");
        let error = reapply(&root, MOD_NAME, &sources(&dir.path().join("staging"))).unwrap_err();
        assert!(matches!(error, ModError::BackupTampered { .. }), "{error}");
        // Nothing was installed over the top.
        assert!(ModManifest::is_installed(&root));
    }

    #[test]
    fn a_scan_finds_manifests_and_ignores_the_backup_tree() {
        let dir = tempfile::tempdir().unwrap();
        let games = dir.path().join("games");
        let modded = games.join("epic/Gizmo/Binaries/Win64");
        fs::create_dir_all(&modded).unwrap();
        game(&modded);
        ModManifest::install(&modded, MOD_NAME, &sources(&dir.path().join("staging"))).unwrap();

        let plain = games.join("gog/Sugar");
        fs::create_dir_all(&plain).unwrap();
        write(&plain.join("Sugar.exe"), b"the game");

        let found = find_manifests(&games);
        assert_eq!(found, vec![modded.clone()], "{found:?}");

        // A `.gl-backup` directory is never itself treated as a game directory,
        // so one manifest is found no matter how many exist on disk.
        let findings = check_roots(&[games.clone(), games]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].ok());
    }

    #[test]
    fn a_plain_tree_has_no_findings() {
        let dir = tempfile::tempdir().unwrap();
        let games = dir.path().join("games");
        fs::create_dir_all(games.join("steam/Portal 2")).unwrap();
        write(&games.join("steam/Portal 2/portal2.exe"), b"the game");
        assert!(check_roots(&[games]).is_empty());
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_manifests(&dir.path().join("nope")).is_empty());
        assert!(check_roots(&[dir.path().join("nope")]).is_empty());
    }
}
