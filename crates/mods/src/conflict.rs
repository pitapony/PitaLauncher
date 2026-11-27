//! The pre-flight conflict scan for the single injection slot.
//!
//! Three of the four injection mechanisms the launcher offers — managed Tier B
//! OptiScaler, HelixSR and DLSSNR — drop files into the game's own directory,
//! and all three can own the same `dxgi.dll`. Before a slot switch (or a mod
//! install) the graphics screen runs this scan: it walks the game's executable
//! directory, matches the file names the mechanisms are known to ship
//! ([`KNOWN_FILES`]), and **names each conflicting file and its owner**.
//!
//! The owner comes from the mod manifest ([`crate::manifest::ModManifest`]),
//! the one ledger a launcher-installed mod writes: a file the manifest records
//! is owned by that manifest's `mod_name`. A file that matches a known name but
//! appears in no manifest is [`Owner::Unmanaged`] — the game itself, or a tool
//! the launcher did not install — and its mechanism falls back to the name's
//! default ([`Mechanism`]). A manifest that exists but cannot be read is a
//! [`ConflictScan::warning`], never a hard failure: the scan still reports what
//! it can see.
//!
//! The module is framework-free and read-only: it writes nothing and it is
//! tested against `tempfile` trees with no game, no GPU and no network. It is
//! the pre-flight, not the enforcement — the one injection slot is enforced by
//! `pitagames_core::profile` and `pitagames_platform::envc` (ADR-0023).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::manifest::ModManifest;

/// One of the mechanisms that can own files in a game directory.
///
/// Every mechanism except Tier A (Proton's own, env-only hook, which writes
/// nothing) appears here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mechanism {
    /// Managed Tier B OptiScaler.
    OptiScaler,
    /// HelixSR, which ships its own OptiScaler copy.
    HelixSr,
    /// DLSSNR-AMD, which ships its own OptiScaler on the `optiscaler` route.
    DlssNr,
    /// ReShade, the feeder routes' loader.
    Reshade,
}

impl Mechanism {
    /// The human name the pre-flight card shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::OptiScaler => "OptiScaler",
            Self::HelixSr => "HelixSR",
            Self::DlssNr => "DLSSNR",
            Self::Reshade => "ReShade",
        }
    }

    /// The mechanism a mod manifest's `mod_name` names, when it is one of ours.
    pub fn from_mod_name(name: &str) -> Option<Self> {
        match name {
            "optiscaler" => Some(Self::OptiScaler),
            "helixsr" => Some(Self::HelixSr),
            "dlssnr" => Some(Self::DlssNr),
            "reshade" => Some(Self::Reshade),
            _ => None,
        }
    }
}

/// One file name a mechanism is known to ship, in the game's executable dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownFile {
    /// The name to match (compared exactly; Windows game directories keep the
    /// names OptiScaler/HelixSR/DLSSNR ship).
    pub name: &'static str,
    /// `true` when the entry is a directory.
    pub directory: bool,
    /// The mechanism this name belongs to when no manifest overrides it.
    pub mechanism: Mechanism,
}

/// The known injection files, in a stable order.
///
/// The DLL names are the ones [`crate::tierb::DllName::CANDIDATES`] can pick,
/// plus the loaders' own configs, folders and logs. A name prepended by a
/// manifest is attributed to that manifest's mechanism regardless of this table.
pub const KNOWN_FILES: &[KnownFile] = &[
    // OptiScaler / Tier B (and the copy HelixSR / DLSSNR ship under the same
    // names — the manifest disambiguates).
    KnownFile {
        name: "dxgi.dll",
        directory: false,
        mechanism: Mechanism::OptiScaler,
    },
    KnownFile {
        name: "d3d12.dll",
        directory: false,
        mechanism: Mechanism::OptiScaler,
    },
    KnownFile {
        name: "dbghelp.dll",
        directory: false,
        mechanism: Mechanism::OptiScaler,
    },
    KnownFile {
        name: "winmm.dll",
        directory: false,
        mechanism: Mechanism::OptiScaler,
    },
    KnownFile {
        name: "version.dll",
        directory: false,
        mechanism: Mechanism::OptiScaler,
    },
    KnownFile {
        name: "OptiScaler.ini",
        directory: false,
        mechanism: Mechanism::OptiScaler,
    },
    KnownFile {
        name: "OptiScaler",
        directory: true,
        mechanism: Mechanism::OptiScaler,
    },
    KnownFile {
        name: "OptiScaler.log",
        directory: false,
        mechanism: Mechanism::OptiScaler,
    },
    // HelixSR's own files (its Scaler also ships OptiScaler under the names
    // above; the manifest is what attributes those to HelixSR).
    KnownFile {
        name: "HelixSR",
        directory: true,
        mechanism: Mechanism::HelixSr,
    },
    KnownFile {
        name: "helixsr.dll",
        directory: false,
        mechanism: Mechanism::HelixSr,
    },
    KnownFile {
        name: "helixsr.ini",
        directory: false,
        mechanism: Mechanism::HelixSr,
    },
    KnownFile {
        name: "HLX_FSR.log",
        directory: false,
        mechanism: Mechanism::HelixSr,
    },
    // DLSSNR-AMD.
    KnownFile {
        name: "dlssnr-amd.asi",
        directory: false,
        mechanism: Mechanism::DlssNr,
    },
    KnownFile {
        name: "dlssnr-amd.ini",
        directory: false,
        mechanism: Mechanism::DlssNr,
    },
    KnownFile {
        name: "dlssnr-amd.log",
        directory: false,
        mechanism: Mechanism::DlssNr,
    },
    KnownFile {
        name: "nvngx_dlssnr.dll",
        directory: false,
        mechanism: Mechanism::DlssNr,
    },
    // ReShade (the post-v1 feeder routes).
    KnownFile {
        name: "ReShade.ini",
        directory: false,
        mechanism: Mechanism::Reshade,
    },
    KnownFile {
        name: "ReShade64.dll",
        directory: false,
        mechanism: Mechanism::Reshade,
    },
    KnownFile {
        name: "ReShade32.dll",
        directory: false,
        mechanism: Mechanism::Reshade,
    },
    KnownFile {
        name: "reshade-shaders",
        directory: true,
        mechanism: Mechanism::Reshade,
    },
];

/// Who owns a conflicting file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Owner {
    /// A mod the launcher installed, named by its manifest, with the mechanism
    /// that manifest's `mod_name` implies (when it names one we know).
    Mod {
        /// The manifest's `mod_name`.
        name: String,
        /// The mechanism the name implies, when it names one of ours.
        #[serde(skip_serializing_if = "Option::is_none")]
        mechanism: Option<Mechanism>,
    },
    /// A file no manifest records: the game itself, or a tool the launcher did
    /// not install.
    Unmanaged,
}

impl Owner {
    /// The one-line owner label the pre-flight card shows.
    pub fn label(&self) -> String {
        match self {
            Self::Mod { name, .. } => format!("{name} (installed by the launcher)"),
            Self::Unmanaged => "not installed by the launcher".to_string(),
        }
    }
}

/// One known injection file that exists in the game directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictFile {
    /// The absolute path that matched.
    pub path: PathBuf,
    /// The known name that matched.
    pub name: &'static str,
    /// The mechanism the file belongs to: the manifest owner's mechanism when
    /// the file is managed, else the name's default.
    pub mechanism: Mechanism,
    /// The owner of the file.
    pub owner: Owner,
    /// Whether a manifest records the file (the launcher installed it).
    pub managed: bool,
}

/// The result of scanning a game directory for injection files.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct ConflictScan {
    /// Every known file found, ordered by path.
    pub files: Vec<ConflictFile>,
    /// The manifest's `mod_name`, when one is installed.
    pub installed_mod: Option<String>,
    /// A non-fatal warning (a manifest that could not be read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl ConflictScan {
    /// Whether nothing conflicting was found.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The distinct mechanisms present in the game directory.
    pub fn mechanisms(&self) -> BTreeSet<Mechanism> {
        self.files.iter().map(|file| file.mechanism).collect()
    }

    /// Whether files from more than one injection mechanism are present.
    ///
    /// Two mechanisms in one directory is exactly the state the single slot
    /// forbids (ADR-0023); the launcher never creates it itself — a manifest
    /// install refuses a second mod — so a hit names an unmanaged leftover.
    pub fn has_conflict(&self) -> bool {
        self.mechanisms().len() > 1
    }

    /// The files a manifest records (the ones the launcher can uninstall).
    pub fn managed_files(&self) -> Vec<&ConflictFile> {
        self.files.iter().filter(|file| file.managed).collect()
    }

    /// Every owner named by the scan, distinct and in first-seen order.
    pub fn owners(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut owners = Vec::new();
        for file in &self.files {
            let label = file.owner.label();
            if seen.insert(label.clone()) {
                owners.push(label);
            }
        }
        owners
    }

    /// One line per file: the path, its mechanism and its owner.
    pub fn summary(&self) -> Vec<String> {
        self.files
            .iter()
            .map(|file| {
                format!(
                    "{} — {} ({}, {})",
                    file.path.display(),
                    file.name,
                    file.mechanism.label(),
                    file.owner.label()
                )
            })
            .collect()
    }
}

/// Scan the game directory that holds the executable for injection files.
///
/// `target` is the game's executable or its install directory; an Unreal game's
/// `*/Binaries/Win64` layout is resolved the same way a Tier B deploy resolves
/// it ([`crate::tierb::resolve_exe_dir`]), so the pre-flight and the install
/// agree about where files land. A path that is neither a file nor a directory
/// yields an empty scan with the resolution error as its warning.
#[must_use]
pub fn scan(target: &Path) -> ConflictScan {
    match crate::tierb::resolve_exe_dir(target) {
        Ok(dir) => scan_dir(&dir),
        Err(error) => ConflictScan {
            warning: Some(error.to_string()),
            ..ConflictScan::default()
        },
    }
}

/// Scan an already-resolved executable directory.
///
/// The caller that has resolved the directory (the graphics screen, the CLI)
/// uses this to avoid resolving twice.
#[must_use]
pub fn scan_dir(exe_dir: &Path) -> ConflictScan {
    let mut scan = ConflictScan::default();

    let manifest = match ModManifest::load(exe_dir) {
        Ok(None) => None,
        Ok(Some(manifest)) => {
            scan.installed_mod = Some(manifest.mod_name.clone());
            Some(manifest)
        }
        Err(error) => {
            scan.warning = Some(error.to_string());
            None
        }
    };

    for known in KNOWN_FILES {
        let path = exe_dir.join(known.name);
        let present = if known.directory {
            path.is_dir()
        } else {
            path.is_file()
        };
        if !present {
            continue;
        }

        let managed = manifest
            .as_ref()
            .is_some_and(|manifest| manifest_records(manifest, known));
        let owner = match (&manifest, managed) {
            (Some(manifest), true) => Owner::Mod {
                name: manifest.mod_name.clone(),
                mechanism: Mechanism::from_mod_name(&manifest.mod_name),
            },
            _ => Owner::Unmanaged,
        };
        let mechanism = match &owner {
            Owner::Mod {
                mechanism: Some(mechanism),
                ..
            } => *mechanism,
            _ => known.mechanism,
        };

        scan.files.push(ConflictFile {
            path,
            name: known.name,
            mechanism,
            owner,
            managed,
        });
    }

    scan.files.sort_by(|left, right| left.path.cmp(&right.path));
    scan
}

/// Whether a manifest records a known file (a file that is it, or lives inside
/// it when the known file is a directory).
fn manifest_records(manifest: &ModManifest, known: &KnownFile) -> bool {
    let prefix = format!("{}/", known.name);
    manifest.entries.iter().any(|entry| {
        entry.path == known.name || (known.directory && entry.path.starts_with(&prefix))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ModManifest, ModSource};
    use std::fs;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }

    fn mkdir(path: &Path) {
        fs::create_dir_all(path).unwrap();
    }

    /// Install a manifest for one mod, recording the given game-relative paths.
    fn install_manifest(root: &Path, mod_name: &str, paths: &[&str]) {
        let sources: Vec<ModSource> = paths
            .iter()
            .map(|path| {
                let src = root.join("_src").join(path.replace('/', "_"));
                touch(&src);
                ModSource::new(*path, src)
            })
            .collect();
        ModManifest::install(root, mod_name, &sources).unwrap();
    }

    #[test]
    fn a_clean_directory_has_no_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Game.exe"));

        let scan = scan_dir(dir.path());
        assert!(scan.is_empty());
        assert!(!scan.has_conflict());
        assert_eq!(scan.installed_mod, None);
        assert!(scan.warning.is_none());
        assert!(scan.summary().is_empty());
    }

    #[test]
    fn a_managed_file_is_attributed_to_its_manifest_mod() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Game.exe"));
        install_manifest(dir.path(), "optiscaler", &["dxgi.dll", "OptiScaler.ini"]);

        let scan = scan_dir(dir.path());
        assert_eq!(scan.installed_mod.as_deref(), Some("optiscaler"));
        assert_eq!(scan.files.len(), 2);
        assert!(!scan.has_conflict(), "one mechanism is not a conflict");

        let dll = scan
            .files
            .iter()
            .find(|file| file.name == "dxgi.dll")
            .unwrap();
        assert!(dll.managed);
        assert_eq!(dll.mechanism, Mechanism::OptiScaler);
        assert_eq!(
            dll.owner,
            Owner::Mod {
                name: "optiscaler".to_string(),
                mechanism: Some(Mechanism::OptiScaler),
            }
        );
        assert!(dll.owner.label().contains("optiscaler"));
        assert_eq!(scan.managed_files().len(), 2);
    }

    #[test]
    fn an_unmanaged_file_is_reported_without_an_owner() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("ReShade.ini"));

        let scan = scan_dir(dir.path());
        assert_eq!(scan.files.len(), 1);
        let file = &scan.files[0];
        assert!(!file.managed);
        assert_eq!(file.owner, Owner::Unmanaged);
        assert_eq!(file.mechanism, Mechanism::Reshade);
        assert!(file.owner.label().contains("not installed"));
    }

    #[test]
    fn two_mechanisms_are_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("OptiScaler.ini"));
        touch(&dir.path().join("ReShade.ini"));

        let scan = scan_dir(dir.path());
        assert!(scan.has_conflict());
        let mechanisms = scan.mechanisms();
        assert!(mechanisms.contains(&Mechanism::OptiScaler));
        assert!(mechanisms.contains(&Mechanism::Reshade));
        // The summary names each file and its owner.
        assert_eq!(scan.summary().len(), 2);
    }

    #[test]
    fn a_helixsr_manifest_attributes_the_shared_dll_to_helixsr() {
        // HelixSR ships its own OptiScaler, so it can own `dxgi.dll` too. The
        // manifest is what tells the two apart.
        let dir = tempfile::tempdir().unwrap();
        install_manifest(dir.path(), "helixsr", &["dxgi.dll", "HelixSR/helixsr.dll"]);

        let scan = scan_dir(dir.path());
        let dll = scan
            .files
            .iter()
            .find(|file| file.name == "dxgi.dll")
            .unwrap();
        assert_eq!(dll.mechanism, Mechanism::HelixSr);
        assert!(dll.managed);
        // The HelixSR folder is recorded by a file inside it.
        let folder = scan
            .files
            .iter()
            .find(|file| file.name == "HelixSR")
            .unwrap();
        assert!(folder.managed);
        assert_eq!(folder.mechanism, Mechanism::HelixSr);
    }

    #[test]
    fn a_malformed_manifest_is_a_warning_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("dxgi.dll"));
        mkdir(&dir.path().join(crate::manifest::BACKUP_DIR));
        fs::write(
            dir.path()
                .join(crate::manifest::BACKUP_DIR)
                .join(crate::manifest::MANIFEST_NAME),
            b"not json",
        )
        .unwrap();

        let scan = scan_dir(dir.path());
        assert!(scan.warning.is_some());
        assert_eq!(scan.installed_mod, None);
        // The file is still reported, as unmanaged.
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].owner, Owner::Unmanaged);
    }

    #[test]
    fn an_unreal_binaries_directory_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let win64 = dir.path().join("Gizmo/Binaries/Win64");
        mkdir(&win64);
        touch(&win64.join("OptiScaler.ini"));

        // Scanning the project directory resolves into `Binaries/Win64`, the
        // same place a Tier B deploy writes.
        let scan = scan(&dir.path().join("Gizmo"));
        assert_eq!(scan.files.len(), 1);
        assert!(
            scan.files[0]
                .path
                .ends_with("Binaries/Win64/OptiScaler.ini"),
            "{:?}",
            scan.files[0].path
        );
    }

    #[test]
    fn a_missing_target_is_an_empty_scan_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let scan = scan(&dir.path().join("missing"));
        assert!(scan.is_empty());
        assert!(scan.warning.is_some());
    }

    #[test]
    fn mechanisms_map_from_the_mod_names_we_use() {
        assert_eq!(
            Mechanism::from_mod_name("optiscaler"),
            Some(Mechanism::OptiScaler)
        );
        assert_eq!(
            Mechanism::from_mod_name("helixsr"),
            Some(Mechanism::HelixSr)
        );
        assert_eq!(Mechanism::from_mod_name("dlssnr"), Some(Mechanism::DlssNr));
        assert_eq!(Mechanism::from_mod_name("other"), None);
    }

    #[test]
    fn the_scan_serializes_the_shape_the_cli_parses() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("dxgi.dll"));
        install_manifest(dir.path(), "optiscaler", &["dxgi.dll"]);

        let scan = scan_dir(dir.path());
        let json = serde_json::to_string(&scan).unwrap();
        assert!(json.contains(r#""installed_mod":"optiscaler""#), "{json}");
        assert!(json.contains(r#""mechanism":"opti_scaler""#), "{json}");
        assert!(json.contains(r#""kind":"mod""#), "{json}");
        // The path the scan found is the game-relative one the CLI reports.
        assert!(json.contains("dxgi.dll"), "{json}");
    }
}
