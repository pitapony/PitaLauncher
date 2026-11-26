//! One manifest format for every mod: what a mod created, what it moved to
//! `.gl-backup/`, and the tree hashes that prove it.
//!
//! The three mod systems PitaGames deploys — Tier B OptiScaler, HelixSR and
//! DLSSNR-AMD — all drop files into a game directory over whatever was already
//! there. Rather than each one inventing its own "did I back this up?" ledger,
//! they all write a [`ModManifest`]: the BLAKE3 hash of every file they created,
//! the BLAKE3 hash of every original they moved into `<game>/.gl-backup/`, and
//! the BLAKE3 tree hash of the game directory before the mod touched it.
//!
//! Installation is [`ModManifest::install`]: it hashes the tree, backs up every
//! destination that already exists, writes the mod's files, hashes them, and
//! writes the manifest last. Uninstallation is [`ModManifest::restore`]: it
//! *verifies* the backups first (a tampered or missing backup is reported, never
//! silently restored), deletes what the mod created, moves the originals back,
//! and reports whether the directory hash came back to the pre-install value.
//!
//! Framework-free and path-explicit, like `pitagames-platform`'s modules: tests
//! point at a `tempfile` tree and never touch the host.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use walkdir::WalkDir;

/// The directory a mod moves replaced originals into, beside the game tree.
pub const BACKUP_DIR: &str = ".gl-backup";

/// The manifest file's name, inside [`BACKUP_DIR`].
pub const MANIFEST_NAME: &str = "manifest.json";

/// The manifest format version, so a future change can be refused loudly.
pub const MANIFEST_VERSION: u32 = 1;

/// What can go wrong installing, verifying or restoring a mod.
#[derive(Debug, thiserror::Error)]
pub enum ModError {
    /// A filesystem operation failed; `action` names it for the reader.
    #[error("cannot {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A mod path was not a safe relative path.
    #[error("mod path `{0}` is not a safe relative path")]
    UnsafePath(String),
    /// A mod would write over a directory.
    #[error("mod destination `{0}` is a directory")]
    DestinationIsDirectory(String),
    /// A manifest already exists, so a second install would clobber it.
    #[error("a mod (`{0}`) is already installed; uninstall it first")]
    AlreadyInstalled(String),
    /// A [[`ModManifest::restore`]] could not find the manifest it needs.
    #[error("no mod manifest at {}", path.display())]
    NoManifest {
        /// The game directory that was searched.
        path: PathBuf,
    },
    /// A manifest file exists but is not readable as a manifest.
    #[error("the mod manifest at {} is malformed: {detail}", path.display())]
    MalformedManifest {
        /// The manifest file.
        path: PathBuf,
        /// Why it could not be parsed.
        detail: String,
    },
    /// A restore found a file whose backup is missing or wrong.
    #[error("cannot restore `{path}`: its backup is {problem}")]
    BackupTampered {
        /// The game-relative path whose backup is bad.
        path: String,
        /// `missing` or `modified`.
        problem: &'static str,
    },
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> ModError {
    ModError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

/// One file a mod wants to place, and where its content comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSource {
    /// Destination relative to the game directory, with forward slashes.
    pub path: String,
    /// The file whose bytes are copied into the game directory.
    pub source: PathBuf,
}

impl ModSource {
    /// A destination path and the file that fills it.
    pub fn new(path: impl Into<String>, source: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
        }
    }
}

/// One path a manifest remembers: what the mod wrote there and/or what was
/// there before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModEntry {
    /// Destination relative to the game directory, with forward slashes.
    pub path: String,
    /// BLAKE3 of the file the mod wrote, if it wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// BLAKE3 of the original moved into [`BACKUP_DIR`], if there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// What a mod did to one game directory: the files it wrote, the originals it
/// moved to [`BACKUP_DIR`], and the tree hashes either side of the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModManifest {
    /// The manifest format version; a future format is refused at load.
    pub version: u32,
    /// The mod's name, e.g. `optiscaler`.
    pub mod_name: String,
    /// When the mod was installed.
    #[serde(with = "time::serde::rfc3339")]
    pub installed_at: OffsetDateTime,
    /// BLAKE3 tree hash of the game directory before the mod touched it. A
    /// restore that reaches this value is byte-for-byte complete.
    pub pre_install_tree_hash: String,
    /// BLAKE3 tree hash right after the install.
    pub post_install_tree_hash: String,
    /// Directories the install had to create, removed again if left empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_dirs: Vec<String>,
    /// One entry per path, sorted, so the manifest is deterministic.
    pub entries: Vec<ModEntry>,
}

/// The manifest file for a game directory.
fn manifest_path(root: &Path) -> PathBuf {
    root.join(BACKUP_DIR).join(MANIFEST_NAME)
}

impl ModManifest {
    /// Whether a manifest exists for `root`.
    pub fn is_installed(root: &Path) -> bool {
        manifest_path(root).is_file()
    }

    /// Read the manifest for `root`, if one is there.
    pub fn load(root: &Path) -> Result<Option<Self>, ModError> {
        let path = manifest_path(root);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("read", &path, error)),
        };
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|error| ModError::MalformedManifest {
                path: path.clone(),
                detail: error.to_string(),
            })?;
        if manifest.version != MANIFEST_VERSION {
            return Err(ModError::MalformedManifest {
                path,
                detail: format!(
                    "unsupported manifest version {} (this build writes {MANIFEST_VERSION})",
                    manifest.version
                ),
            });
        }
        Ok(Some(manifest))
    }

    /// Read the manifest for `root`, or report that there is none.
    pub fn load_required(root: &Path) -> Result<Self, ModError> {
        Self::load(root)?.ok_or_else(|| ModError::NoManifest {
            path: root.to_path_buf(),
        })
    }

    /// Write the manifest into `<root>/.gl-backup/manifest.json`.
    pub fn save(&self, root: &Path) -> Result<(), ModError> {
        let path = manifest_path(root);
        let dir = path
            .parent()
            .expect("the manifest path always has a parent directory");
        fs::create_dir_all(dir).map_err(|error| io_error("create directory", dir, error))?;
        let json =
            serde_json::to_string_pretty(self).map_err(|error| ModError::MalformedManifest {
                path: path.clone(),
                detail: error.to_string(),
            })?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, json).map_err(|error| io_error("write", &temp, error))?;
        fs::rename(&temp, &path).map_err(|error| io_error("replace", &path, error))
    }

    /// Install a mod into `root`.
    ///
    /// Hashes the tree, backs up every destination that already exists, writes
    /// the mod's files and hashes them, then writes the manifest last. A second
    /// install is refused until the first is restored.
    pub fn install(root: &Path, mod_name: &str, sources: &[ModSource]) -> Result<Self, ModError> {
        if let Some(existing) = Self::load(root)? {
            return Err(ModError::AlreadyInstalled(existing.mod_name));
        }
        // Validate every path before touching the tree, so a refused install
        // cannot leave a half-written mod behind.
        for source in sources {
            if !safe_relative(&source.path) {
                return Err(ModError::UnsafePath(source.path.clone()));
            }
        }

        let pre = tree_hash(root)?;
        let backup_root = root.join(BACKUP_DIR);
        let mut created_dirs = Vec::new();
        let mut entries = Vec::new();

        for source in sources {
            let dest = join_relative(root, &source.path);
            if dest.is_dir() {
                return Err(ModError::DestinationIsDirectory(source.path.clone()));
            }

            let backup_hash = if dest.exists() {
                let original = fs::read(&dest).map_err(|error| io_error("read", &dest, error))?;
                let backup = join_relative(&backup_root, &source.path);
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| io_error("create directory", parent, error))?;
                }
                fs::rename(&dest, &backup)
                    .map_err(|error| io_error("move to backup", &dest, error))?;
                Some(blake3::hash(&original).to_hex().to_string())
            } else {
                None
            };

            if let Some(parent) = dest.parent() {
                create_parent_dirs(root, parent, &mut created_dirs)?;
            }
            let bytes = fs::read(&source.source)
                .map_err(|error| io_error("read", &source.source, error))?;
            fs::write(&dest, &bytes).map_err(|error| io_error("write", &dest, error))?;
            let created = blake3::hash(&bytes).to_hex().to_string();

            entries.push(ModEntry {
                path: source.path.clone(),
                created: Some(created),
                backup: backup_hash,
            });
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        created_dirs.sort();
        created_dirs.dedup();

        let post = tree_hash(root)?;
        let manifest = Self {
            version: MANIFEST_VERSION,
            mod_name: mod_name.to_string(),
            installed_at: OffsetDateTime::now_utc(),
            pre_install_tree_hash: pre,
            post_install_tree_hash: post,
            created_dirs,
            entries,
        };
        manifest.save(root)?;
        Ok(manifest)
    }
}

/// Join a forward-slash relative path onto a root, one component at a time.
fn join_relative(root: &Path, relative: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
    }
    path
}

/// Create each missing directory under `root` down to `dir`, recording the ones
/// this install created so a restore can remove them again.
fn create_parent_dirs(root: &Path, dir: &Path, created: &mut Vec<String>) -> Result<(), ModError> {
    let relative = dir.strip_prefix(root).unwrap_or(dir);
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part.as_os_str());
        if !current.exists() {
            fs::create_dir(&current)
                .map_err(|error| io_error("create directory", &current, error))?;
            created.push(relative_forward(root, &current));
        }
    }
    Ok(())
}

/// One way an installed mod no longer matches its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModIssue {
    /// A file the mod created is gone.
    MissingCreated { path: String },
    /// A file the mod created is different now.
    ModifiedCreated {
        path: String,
        expected: String,
        actual: String,
    },
    /// A backup the mod made is gone.
    MissingBackup { path: String },
    /// A backup the mod made is different now.
    TamperedBackup {
        path: String,
        expected: String,
        actual: String,
    },
    /// The directory hash is not the one the manifest recorded.
    TreeHashMismatch { expected: String, actual: String },
}

impl std::fmt::Display for ModIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCreated { path } => write!(f, "{path}: created file is missing"),
            Self::ModifiedCreated {
                path,
                expected,
                actual,
            } => write!(
                f,
                "{path}: created file was modified (expected {expected}, found {actual})"
            ),
            Self::MissingBackup { path } => write!(f, "{path}: backup is missing"),
            Self::TamperedBackup {
                path,
                expected,
                actual,
            } => write!(
                f,
                "{path}: backup was tampered with (expected {expected}, found {actual})"
            ),
            Self::TreeHashMismatch { expected, actual } => write!(
                f,
                "tree hash {actual} does not match the recorded {expected}"
            ),
        }
    }
}

/// What [`ModManifest::verify`] found: every file no longer matching the
/// manifest, and nothing silently ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    /// Every mismatch, in path order.
    pub issues: Vec<ModIssue>,
}

impl VerifyReport {
    /// Whether the installed mod matches its manifest exactly.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

/// What a restore did, and whether the tree came back byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreReport {
    /// The mod that was removed.
    pub mod_name: String,
    /// Files the mod created that were deleted.
    pub removed: Vec<String>,
    /// Originals moved back out of [`BACKUP_DIR`].
    pub restored: Vec<String>,
    /// The tree hash after the restore.
    pub tree_hash: String,
    /// The pre-install tree hash the restore aimed for.
    pub expected_tree_hash: String,
    /// Whether `tree_hash` equals `expected_tree_hash`.
    pub tree_hash_ok: bool,
    /// Anything still wrong after the restore.
    pub issues: Vec<ModIssue>,
}

impl ModManifest {
    /// Verify the installed mod against its manifest, changing nothing.
    #[must_use]
    pub fn verify(&self, root: &Path) -> VerifyReport {
        let backup_root = root.join(BACKUP_DIR);
        let mut issues = Vec::new();

        for entry in &self.entries {
            if let Some(expected) = &entry.created {
                let dest = join_relative(root, &entry.path);
                match fs::read(&dest) {
                    Ok(bytes) => {
                        let actual = blake3::hash(&bytes).to_hex().to_string();
                        if &actual != expected {
                            issues.push(ModIssue::ModifiedCreated {
                                path: entry.path.clone(),
                                expected: expected.clone(),
                                actual,
                            });
                        }
                    }
                    Err(_) => issues.push(ModIssue::MissingCreated {
                        path: entry.path.clone(),
                    }),
                }
            }
            if let Some(expected) = &entry.backup {
                let backup = join_relative(&backup_root, &entry.path);
                match fs::read(&backup) {
                    Ok(bytes) => {
                        let actual = blake3::hash(&bytes).to_hex().to_string();
                        if &actual != expected {
                            issues.push(ModIssue::TamperedBackup {
                                path: entry.path.clone(),
                                expected: expected.clone(),
                                actual,
                            });
                        }
                    }
                    Err(_) => issues.push(ModIssue::MissingBackup {
                        path: entry.path.clone(),
                    }),
                }
            }
        }

        // A whole-tree check catches a file that was added or removed outside
        // the manifest, which no per-file comparison can see.
        if let Ok(actual) = tree_hash(root)
            && actual != self.post_install_tree_hash
        {
            issues.push(ModIssue::TreeHashMismatch {
                expected: self.post_install_tree_hash.clone(),
                actual,
            });
        }

        VerifyReport { issues }
    }

    /// Restore the game directory to its pre-install state.
    ///
    /// Refuses before changing anything if a backup is missing or its hash does
    /// not match, so a tampered backup is reported instead of restored over.
    /// Otherwise it deletes what the mod created, moves the originals back,
    /// removes the directories it created, and reports whether the tree hash
    /// returned to the pre-install value.
    pub fn restore(root: &Path) -> Result<RestoreReport, ModError> {
        let manifest = Self::load_required(root)?;
        let backup_root = root.join(BACKUP_DIR);

        // Check every backup before touching the tree: a restore that put back
        // the wrong bytes would be worse than no restore at all.
        for entry in &manifest.entries {
            let Some(expected) = &entry.backup else {
                continue;
            };
            let backup = join_relative(&backup_root, &entry.path);
            if !backup.is_file() {
                return Err(ModError::BackupTampered {
                    path: entry.path.clone(),
                    problem: "missing",
                });
            }
            if &file_hash(&backup)? != expected {
                return Err(ModError::BackupTampered {
                    path: entry.path.clone(),
                    problem: "modified",
                });
            }
        }

        // Delete what the mod created.
        let mut removed = Vec::new();
        for entry in &manifest.entries {
            if entry.created.is_none() {
                continue;
            }
            let dest = join_relative(root, &entry.path);
            match fs::remove_file(&dest) {
                Ok(()) => removed.push(entry.path.clone()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("remove", &dest, error)),
            }
        }

        // Move the originals back.
        let mut restored = Vec::new();
        for entry in &manifest.entries {
            if entry.backup.is_none() {
                continue;
            }
            let backup = join_relative(&backup_root, &entry.path);
            let dest = join_relative(root, &entry.path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| io_error("create directory", parent, error))?;
            }
            fs::rename(&backup, &dest).map_err(|error| io_error("restore", &dest, error))?;
            restored.push(entry.path.clone());
        }

        // Remove the directories the install created, deepest first, so an
        // uninstall leaves no empty scaffolding. A directory that still holds
        // something (a restored file, an unrelated file) stays.
        for dir in manifest.created_dirs.iter().rev() {
            let _ = fs::remove_dir(join_relative(root, dir));
        }
        // And the manifest's own directory.
        let _ = fs::remove_file(manifest_path(root));
        let _ = fs::remove_dir(&backup_root);

        let tree_hash = tree_hash(root)?;
        let tree_hash_ok = tree_hash == manifest.pre_install_tree_hash;
        let issues = if tree_hash_ok {
            Vec::new()
        } else {
            vec![ModIssue::TreeHashMismatch {
                expected: manifest.pre_install_tree_hash.clone(),
                actual: tree_hash.clone(),
            }]
        };

        Ok(RestoreReport {
            mod_name: manifest.mod_name,
            removed,
            restored,
            tree_hash,
            expected_tree_hash: manifest.pre_install_tree_hash,
            tree_hash_ok,
            issues,
        })
    }
}

/// The BLAKE3 hash of one file, as lowercase hex.
pub fn file_hash(path: &Path) -> Result<String, ModError> {
    let bytes = fs::read(path).map_err(|error| io_error("read", path, error))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// The BLAKE3 hash of a directory's contents, as lowercase hex.
///
/// Only regular files contribute: each one is fed in as its forward-slash
/// relative path, a NUL separator, its length and its bytes, in sorted path
/// order, so the result depends on *which* files exist and *what* they contain
/// and on nothing else. Directories (and therefore an empty directory left
/// behind by an uninstall) do not change the hash, and the [`BACKUP_DIR`] the
/// manifest itself lives in is skipped. Symlinks contribute nothing and are
/// never followed.
pub fn tree_hash(root: &Path) -> Result<String, ModError> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_backup_entry(entry))
    {
        let entry = entry.map_err(|error| ModError::Io {
            action: "walk",
            path: error
                .path()
                .map_or_else(|| root.to_path_buf(), Path::to_path_buf),
            source: error
                .into_io_error()
                .unwrap_or_else(|| io::Error::other("failed to walk the directory")),
        })?;
        if entry.file_type().is_file() {
            files.push((
                relative_forward(root, entry.path()),
                entry.path().to_path_buf(),
            ));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    for (relative, path) in files {
        let bytes = fs::read(&path).map_err(|error| io_error("read", &path, error))?;
        hasher.update(b"file\0");
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The backup directory at the root of the tree, which never hashes.
fn is_backup_entry(entry: &walkdir::DirEntry) -> bool {
    entry.depth() == 1 && entry.file_name() == std::ffi::OsStr::new(BACKUP_DIR)
}

/// The forward-slash path of `path` relative to `root`.
fn relative_forward(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Whether `relative` is a safe mod path: one relative component chain with no
/// `..`, no escape, and nothing inside [`BACKUP_DIR`].
fn safe_relative(relative: &str) -> bool {
    if relative.is_empty() || relative.starts_with('/') || relative.ends_with('/') {
        return false;
    }
    let mut parts = relative.split('/');
    let first = parts.next();
    first != Some(BACKUP_DIR)
        && first.is_some_and(|part| !part.is_empty() && part != "." && part != "..")
        && parts.all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn fixture(root: &Path) {
        write(&root.join("game.exe"), b"the game");
        write(&root.join("dxgi.dll"), b"an old dll");
        write(&root.join("data/pack.bin"), b"pack bytes");
    }

    #[test]
    fn tree_hash_tracks_contents_not_walk_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);

        let first = tree_hash(root).unwrap();
        // Same content, written in a different order, hashes the same.
        let other = tempfile::tempdir().unwrap();
        write(&other.path().join("data/pack.bin"), b"pack bytes");
        write(&other.path().join("dxgi.dll"), b"an old dll");
        write(&other.path().join("game.exe"), b"the game");
        assert_eq!(first, tree_hash(other.path()).unwrap());

        // A changed byte changes it.
        write(&root.join("dxgi.dll"), b"some other dll");
        assert_ne!(first, tree_hash(root).unwrap());

        // So does a new file.
        write(&root.join("new.dll"), b"new");
        assert_ne!(first, tree_hash(root).unwrap());

        // An empty directory does not.
        let before = tree_hash(root).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        assert_eq!(before, tree_hash(root).unwrap());
    }

    #[test]
    fn tree_hash_ignores_the_backup_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);
        let clean = tree_hash(root).unwrap();

        // The manifest and its backups live in `.gl-backup/`, which must never
        // be part of the hash it records.
        write(&root.join(BACKUP_DIR).join(MANIFEST_NAME), b"{}");
        write(&root.join(BACKUP_DIR).join("dxgi.dll"), b"an old dll");
        assert_eq!(clean, tree_hash(root).unwrap());
    }

    #[test]
    fn file_hash_is_stable_and_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        write(&path, b"abc");
        let abc = file_hash(&path).unwrap();
        assert_eq!(abc, file_hash(&path).unwrap());
        write(&path, b"abd");
        assert_ne!(abc, file_hash(&path).unwrap());

        // The published BLAKE3 digest of the empty input, from the BLAKE3
        // test vectors — an independent known-good value, not recomputed here.
        let empty = dir.path().join("empty.bin");
        write(&empty, b"");
        assert_eq!(
            file_hash(&empty).unwrap(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    /// Two files a mod installs: one replaces an existing game file, one is new.
    fn mod_sources(staging: &Path) -> Vec<ModSource> {
        let optiscaler = staging.join("OptiScaler.dll");
        let ini = staging.join("OptiScaler.ini");
        write(&optiscaler, b"the new dll");
        write(&ini, b"[OptiScaler]\nEnabled=true\n");
        vec![
            ModSource::new("dxgi.dll", optiscaler),
            ModSource::new("OptiScaler.ini", ini),
        ]
    }

    #[test]
    fn install_records_created_and_backed_up_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);
        let staging = dir.path().join("staging");

        let before = tree_hash(&root).unwrap();
        let manifest = ModManifest::install(&root, "optiscaler", &mod_sources(&staging)).unwrap();

        // The manifest is on disk and readable.
        assert!(ModManifest::is_installed(&root));
        assert_eq!(ModManifest::load(&root).unwrap().as_ref(), Some(&manifest));
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.mod_name, "optiscaler");
        assert_eq!(manifest.pre_install_tree_hash, before);
        assert_ne!(manifest.post_install_tree_hash, before);

        // `dxgi.dll` was replaced: created and backed up.
        let dll = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "dxgi.dll")
            .expect("dxgi.dll is recorded");
        assert!(dll.created.is_some());
        assert_eq!(
            dll.backup.as_deref(),
            Some(
                file_hash(&root.join(BACKUP_DIR).join("dxgi.dll"))
                    .unwrap()
                    .as_str()
            )
        );
        assert_eq!(
            fs::read(root.join(BACKUP_DIR).join("dxgi.dll")).unwrap(),
            b"an old dll"
        );
        assert_eq!(fs::read(root.join("dxgi.dll")).unwrap(), b"the new dll");

        // `OptiScaler.ini` is new: created, nothing backed up.
        let ini = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "OptiScaler.ini")
            .expect("OptiScaler.ini is recorded");
        assert!(ini.created.is_some());
        assert_eq!(ini.backup, None);

        // Nothing else was touched.
        assert_eq!(fs::read(root.join("game.exe")).unwrap(), b"the game");
    }

    #[test]
    fn a_second_install_is_refused_until_the_first_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);

        ModManifest::install(
            &root,
            "optiscaler",
            &mod_sources(&dir.path().join("staging")),
        )
        .unwrap();
        let error = ModManifest::install(
            &root,
            "helixsr",
            &[ModSource::new(
                "dxgi.dll",
                dir.path().join("staging/OptiScaler.dll"),
            )],
        )
        .unwrap_err();
        assert!(matches!(error, ModError::AlreadyInstalled(name) if name == "optiscaler"));
    }

    #[test]
    fn an_unsafe_destination_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);
        let staging = dir.path().join("staging");
        write(&staging.join("evil.dll"), b"evil");

        for path in ["../escape.dll", "/etc/passwd", ".gl-backup/steal", "a/../b"] {
            let error = ModManifest::install(
                &root,
                "optiscaler",
                &[ModSource::new(path, staging.join("evil.dll"))],
            )
            .unwrap_err();
            assert!(
                matches!(&error, ModError::UnsafePath(p) if p == path),
                "{path}: {error}"
            );
        }
        // A refused install left nothing behind.
        assert!(!ModManifest::is_installed(&root));
    }

    #[test]
    fn install_then_restore_returns_the_original_tree_hash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);
        let before = tree_hash(&root).unwrap();

        ModManifest::install(
            &root,
            "optiscaler",
            &mod_sources(&dir.path().join("staging")),
        )
        .unwrap();
        assert_ne!(tree_hash(&root).unwrap(), before);

        let report = ModManifest::restore(&root).unwrap();
        assert_eq!(report.mod_name, "optiscaler");
        assert!(report.tree_hash_ok, "{report:?}");
        assert_eq!(report.tree_hash, before);
        assert_eq!(report.expected_tree_hash, before);
        assert_eq!(report.removed, ["OptiScaler.ini", "dxgi.dll"]);
        assert_eq!(report.restored, ["dxgi.dll"]);
        assert!(report.issues.is_empty());

        // The tree is byte-for-byte the original, and the ledger is gone.
        assert_eq!(tree_hash(&root).unwrap(), before);
        assert_eq!(fs::read(root.join("dxgi.dll")).unwrap(), b"an old dll");
        assert!(!root.join("OptiScaler.ini").exists());
        assert!(!root.join(BACKUP_DIR).exists());
        assert!(!ModManifest::is_installed(&root));
    }

    #[test]
    fn restore_removes_directories_the_install_created() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);
        let before = tree_hash(&root).unwrap();

        ModManifest::install(
            &root,
            "helixsr",
            &[ModSource::new(
                "mods/hlx/x.dll",
                dir.path().join("game/dxgi.dll"),
            )],
        )
        .unwrap();
        assert!(root.join("mods/hlx/x.dll").is_file());

        ModManifest::restore(&root).unwrap();
        assert!(!root.join("mods").exists(), "the created tree is pruned");
        assert_eq!(tree_hash(&root).unwrap(), before);
    }

    #[test]
    fn restore_without_a_manifest_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);

        let error = ModManifest::restore(&root).unwrap_err();
        assert!(matches!(error, ModError::NoManifest { .. }), "{error}");
    }

    #[test]
    fn a_malformed_or_future_manifest_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);

        write(&root.join(BACKUP_DIR).join(MANIFEST_NAME), b"not json");
        let error = ModManifest::load(&root).unwrap_err();
        assert!(
            matches!(error, ModError::MalformedManifest { .. }),
            "{error}"
        );
        // And a restore refuses rather than guessing.
        assert!(ModManifest::restore(&root).is_err());

        // A future version is refused by name, not read as if it were ours.
        let future_dir = tempfile::tempdir().unwrap();
        let (fresh_root, manifest) = installed(future_dir.path());
        let mut future = manifest.clone();
        future.version = MANIFEST_VERSION + 1;
        future.save(&fresh_root).unwrap();
        let error = ModManifest::load(&fresh_root).unwrap_err();
        assert!(
            error.to_string().contains("unsupported manifest version"),
            "{error}"
        );
    }

    fn installed(dir: &Path) -> (PathBuf, ModManifest) {
        let root = dir.join("game");
        fs::create_dir_all(&root).unwrap();
        fixture(&root);
        let manifest =
            ModManifest::install(&root, "optiscaler", &mod_sources(&dir.join("staging"))).unwrap();
        (root, manifest)
    }

    #[test]
    fn verify_accepts_a_clean_install() {
        let dir = tempfile::tempdir().unwrap();
        let (root, manifest) = installed(dir.path());
        assert!(manifest.verify(&root).ok(), "{:?}", manifest.verify(&root));
    }

    #[test]
    fn verify_reports_a_modified_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let (root, manifest) = installed(dir.path());

        write(&root.join("dxgi.dll"), b"a user changed this");

        let report = manifest.verify(&root);
        assert!(!report.ok());
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                ModIssue::ModifiedCreated { path, .. } if path == "dxgi.dll"
            )),
            "{:?}",
            report.issues
        );
        // The whole-tree check also notices.
        assert!(
            report
                .issues
                .iter()
                .any(|issue| matches!(issue, ModIssue::TreeHashMismatch { .. }))
        );
    }

    #[test]
    fn verify_reports_a_missing_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let (root, manifest) = installed(dir.path());
        fs::remove_file(root.join("OptiScaler.ini")).unwrap();

        let report = manifest.verify(&root);
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                ModIssue::MissingCreated { path } if path == "OptiScaler.ini"
            )),
            "{:?}",
            report.issues
        );
    }

    #[test]
    fn verify_reports_a_tampered_backup() {
        let dir = tempfile::tempdir().unwrap();
        let (root, manifest) = installed(dir.path());

        write(&root.join(BACKUP_DIR).join("dxgi.dll"), b"not the original");

        let report = manifest.verify(&root);
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                ModIssue::TamperedBackup { path, .. } if path == "dxgi.dll"
            )),
            "{:?}",
            report.issues
        );

        // A missing backup is reported too.
        fs::remove_file(root.join(BACKUP_DIR).join("dxgi.dll")).unwrap();
        let report = manifest.verify(&root);
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                ModIssue::MissingBackup { path } if path == "dxgi.dll"
            )),
            "{:?}",
            report.issues
        );
    }

    #[test]
    fn verify_reports_an_unexpected_file_as_a_tree_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (root, manifest) = installed(dir.path());
        // Every manifest entry still matches, but a file nobody recorded is now
        // in the tree — the tree hash catches it where per-file checks cannot.
        write(&root.join("something-else.dll"), b"added later");

        let report = manifest.verify(&root);
        assert_eq!(
            report.issues,
            [ModIssue::TreeHashMismatch {
                expected: manifest.post_install_tree_hash.clone(),
                actual: tree_hash(&root).unwrap(),
            }]
        );
    }

    #[test]
    fn restore_refuses_a_tampered_backup_without_changing_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (root, _manifest) = installed(dir.path());
        write(&root.join(BACKUP_DIR).join("dxgi.dll"), b"tampered");

        let error = ModManifest::restore(&root).unwrap_err();
        assert!(
            matches!(&error, ModError::BackupTampered { path, problem } if path == "dxgi.dll" && *problem == "modified"),
            "{error}"
        );

        // Nothing was moved: the mod files and the tampered backup are as they were.
        assert_eq!(fs::read(root.join("dxgi.dll")).unwrap(), b"the new dll");
        assert_eq!(
            fs::read(root.join(BACKUP_DIR).join("dxgi.dll")).unwrap(),
            b"tampered"
        );
        assert!(ModManifest::is_installed(&root));
    }
}
