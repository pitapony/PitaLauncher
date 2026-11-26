//! Tier B: managed OptiScaler deployment.
//!
//! Tier A is Proton's own, env-only OptiScaler hook. Tier B is the fallback for
//! a game running under Valve Proton / GE / an older build, or when the user
//! wants OptiScaler's in-game menu: the launcher drops OptiScaler's files into
//! the game's own directory, backs up whatever it displaces, and hands the
//! launch a `WINEDLLOVERRIDES` **env vector** (ADR-0023).
//!
//! This module is the one deploy-mechanism, built out of the shared primitives:
//!
//! - [`crate::release`] fetches the pinned OptiScaler asset and verifies its
//!   SHA-256 against our pin (ADR-0024) — the pin is *ours*, never the API's
//!   digest;
//! - [`crate::ini`] patches the shipped `OptiScaler.ini` line-preservingly;
//! - [`crate::manifest`] records every file, backs up every original to
//!   `.gl-backup/`, and restores the tree byte-for-byte on uninstall.
//!
//! The DLL name is chosen from an ordered candidate list — `dxgi → d3d12 →
//! dbghelp → winmm → version` — skipping any name the game or another mod
//! already owns, so a game that ships its own `dxgi.dll` is never clobbered.
//! Everything is framework-free and path-explicit: a `tempfile` tree and a
//! fixture archive exercise the whole flow with no network and no game.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ini::{IniError, IniSetting, IniSurface};
use crate::manifest::{ModError, ModManifest, ModSource, RestoreReport};
use crate::release::{FetchedRelease, Fetcher, PinnedRelease, ReleaseError};

/// The name a Tier B deployment is recorded under in its manifest.
pub const MOD_NAME: &str = "optiscaler";

/// The env var a Tier B deployment sets, so Wine loads OptiScaler natively.
pub const OVERRIDE_ENV: &str = "WINEDLLOVERRIDES";

/// The OptiScaler DLL as the release archive names it, at the archive root.
pub const RELEASE_DLL: &str = "OptiScaler.dll";

/// The OptiScaler ini as the release archive names it, at the archive root.
pub const RELEASE_INI: &str = "OptiScaler.ini";

/// The directory of extra files (`Licenses/`, support files) at the archive
/// root, copied into the game directory verbatim.
pub const RELEASE_DIR: &str = "OptiScaler";

/// The name OptiScaler's DLL takes in the game directory, in preference order.
///
/// `dxgi.dll` is the best hook; the later names exist so a game that ships one
/// of them does not force us to clobber it (ADR-0023).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DllName {
    /// `dxgi.dll`.
    Dxgi,
    /// `d3d12.dll`.
    D3d12,
    /// `dbghelp.dll`.
    Dbghelp,
    /// `winmm.dll`.
    Winmm,
    /// `version.dll`.
    Version,
}

impl DllName {
    /// The candidate names in preference order, the one list this module walks.
    pub const CANDIDATES: [DllName; 5] = [
        Self::Dxgi,
        Self::D3d12,
        Self::Dbghelp,
        Self::Winmm,
        Self::Version,
    ];

    /// The file name the DLL takes in the game directory.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Dxgi => "dxgi.dll",
            Self::D3d12 => "d3d12.dll",
            Self::Dbghelp => "dbghelp.dll",
            Self::Winmm => "winmm.dll",
            Self::Version => "version.dll",
        }
    }

    /// The name inside the `WINEDLLOVERRIDES` value, without the `.dll`.
    pub const fn override_key(self) -> &'static str {
        match self {
            Self::Dxgi => "dxgi",
            Self::D3d12 => "d3d12",
            Self::Dbghelp => "dbghelp",
            Self::Winmm => "winmm",
            Self::Version => "version",
        }
    }
}

/// A failure deploying or uninstalling a Tier B OptiScaler.
#[derive(Debug, thiserror::Error)]
pub enum TierBError {
    /// The target was neither a file nor a directory.
    #[error("no game directory or executable at {}", path.display())]
    TargetMissing {
        /// The path that was given.
        path: PathBuf,
    },
    /// Every candidate DLL name is already taken in the game directory.
    #[error(
        "no OptiScaler DLL name is free in {} \
         (dxgi.dll, d3d12.dll, dbghelp.dll, winmm.dll and version.dll all exist)",
        dir.display()
    )]
    NoFreeDll {
        /// The game directory.
        dir: PathBuf,
    },
    /// The release archive does not contain a file the deployment needs.
    #[error("the OptiScaler release has no `{name}`")]
    MissingReleaseFile {
        /// The missing file (`OptiScaler.dll` / `OptiScaler.ini`).
        name: String,
    },
    /// Fetching or verifying the release failed.
    #[error(transparent)]
    Release(#[from] ReleaseError),
    /// Patching the ini failed.
    #[error(transparent)]
    Ini(#[from] IniError),
    /// Writing or restoring the manifest failed.
    #[error(transparent)]
    Manifest(#[from] ModError),
    /// A filesystem operation failed; `action` names it for the reader.
    #[error("cannot {action} {}: {source}", path.display())]
    Io {
        /// The operation.
        action: &'static str,
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> TierBError {
    TierBError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

/// A deployed Tier B OptiScaler: where it landed, what it displaced, and the
/// launch env it requires.
#[derive(Debug, Clone)]
pub struct Deployment {
    /// The game directory the files were written into.
    pub exe_dir: PathBuf,
    /// The DLL name that was chosen.
    pub dll: DllName,
    /// The launch env, as a vector of `(key, value)` pairs — never a shell
    /// string (ADR-0023).
    pub env: Vec<(String, String)>,
    /// The fetched and unpacked release.
    pub fetched: FetchedRelease,
    /// The manifest that records the install.
    pub manifest: ModManifest,
}

/// The `WINEDLLOVERRIDES` launch env for a chosen DLL name, as a vector.
pub fn override_env(dll: DllName) -> Vec<(String, String)> {
    vec![(
        OVERRIDE_ENV.to_string(),
        format!("{}=n,b", dll.override_key()),
    )]
}

/// Resolve the directory OptiScaler's files belong in for `target`.
///
/// `target` is the game's executable (the usual input) or its install
/// directory:
///
/// - an executable path resolves to the directory it sits in;
/// - an Unreal game's shipping exe lives under `*/Binaries/Win64/`, and that
///   `Win64` directory is where injection belongs — passing either the exe or
///   the project directory that holds `Binaries/Win64` gets there;
/// - any other directory is used as it is.
pub fn resolve_exe_dir(target: &Path) -> Result<PathBuf, TierBError> {
    if target.is_file() {
        return target
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| TierBError::TargetMissing {
                path: target.to_path_buf(),
            });
    }
    if !target.is_dir() {
        return Err(TierBError::TargetMissing {
            path: target.to_path_buf(),
        });
    }
    if is_unreal_win64(target) {
        return Ok(target.to_path_buf());
    }
    let nested = target.join("Binaries").join("Win64");
    if nested.is_dir() {
        return Ok(nested);
    }
    Ok(target.to_path_buf())
}

/// Whether `dir` is an Unreal `*/Binaries/Win64` directory.
fn is_unreal_win64(dir: &Path) -> bool {
    dir.file_name() == Some(OsStr::new("Win64"))
        && dir.parent().and_then(Path::file_name) == Some(OsStr::new("Binaries"))
}

/// Choose the first free DLL name in preference order.
///
/// A candidate is skipped when the file already exists in the game directory,
/// which is how a name the game (or another mod) owns is honoured: this module
/// never clobbers such a file. When every name is taken the deploy is refused
/// rather than forcing a collision.
pub fn choose_dll(exe_dir: &Path) -> Result<DllName, TierBError> {
    DllName::CANDIDATES
        .into_iter()
        .find(|candidate| !exe_dir.join(candidate.file_name()).exists())
        .ok_or_else(|| TierBError::NoFreeDll {
            dir: exe_dir.to_path_buf(),
        })
}

/// Deploy OptiScaler into the game `target`, using the pinned release and the
/// shared primitives.
///
/// Fetches and SHA-256-verifies `pin`, unpacks it into `staging`, patches its
/// `OptiScaler.ini` with `settings` (line-preserving), copies the DLL under the
/// first free candidate name plus the ini and the `OptiScaler/` folder, writes
/// the manifest, and returns the `WINEDLLOVERRIDES` env vector.
pub fn deploy(
    target: &Path,
    pin: &PinnedRelease,
    fetcher: &dyn Fetcher,
    staging: &Path,
    settings: &[IniSetting],
) -> Result<Deployment, TierBError> {
    let exe_dir = resolve_exe_dir(target)?;
    let dll = choose_dll(&exe_dir)?;
    let fetched = pin.fetch_and_unpack(fetcher, staging)?;

    // The release keeps its two fixed files at the archive root.
    let dll_source = find_root_file(&fetched.files, RELEASE_DLL).ok_or_else(|| {
        TierBError::MissingReleaseFile {
            name: RELEASE_DLL.to_string(),
        }
    })?;
    let ini_source = find_root_file(&fetched.files, RELEASE_INI).map(str::to_string);

    // Patch the shipped ini in place before it is copied, so the game directory
    // only ever sees the already-patched file. A settings list with no ini to
    // patch is a broken release, refused rather than silently dropped.
    match &ini_source {
        Some(ini) => {
            let path = staging.join(ini);
            let text = fs::read_to_string(&path).map_err(|error| io_error("read", &path, error))?;
            let patched = IniSurface::optiscaler().patch(&text, settings)?;
            fs::write(&path, patched).map_err(|error| io_error("write", &path, error))?;
        }
        None if !settings.is_empty() => {
            return Err(TierBError::MissingReleaseFile {
                name: RELEASE_INI.to_string(),
            });
        }
        None => {}
    }

    // The files the game directory gets: the DLL under its chosen name, the
    // ini, and everything inside the release's `OptiScaler/` folder.
    let mut sources = vec![ModSource::new(dll.file_name(), staging.join(dll_source))];
    if let Some(ini) = &ini_source {
        sources.push(ModSource::new(RELEASE_INI, staging.join(ini)));
    }
    let folder_prefix = format!("{RELEASE_DIR}/");
    for file in &fetched.files {
        if file.starts_with(&folder_prefix) {
            sources.push(ModSource::new(file.clone(), staging.join(file)));
        }
    }

    let manifest = ModManifest::install(&exe_dir, MOD_NAME, &sources)?;
    let env = override_env(dll);
    Ok(Deployment {
        exe_dir,
        dll,
        env,
        fetched,
        manifest,
    })
}

/// Uninstall a Tier B OptiScaler, restoring the game tree from its manifest.
pub fn uninstall(target: &Path) -> Result<RestoreReport, TierBError> {
    let exe_dir = resolve_exe_dir(target)?;
    Ok(ModManifest::restore(&exe_dir)?)
}

/// The entry in `files` that is exactly `name` at the archive root.
fn find_root_file<'a>(files: &'a [String], name: &str) -> Option<&'a str> {
    files
        .iter()
        .find(|file| file.as_str() == name)
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    use crate::manifest::tree_hash;
    use crate::release::{MapFetcher, ReleaseError, ReleaseSource, sha256_hex};

    /// Stand-in bytes for the OptiScaler DLL; the deploy never inspects them.
    const DUMMY_DLL: &[u8] = b"not really a PE, but the bytes are what matter";

    /// A stand-in `OptiScaler.ini` with a comment and an ordering to preserve.
    const DUMMY_INI: &str = "\
; OptiScaler.ini — upstream defaults
[Upscalers]
Dx12Upscaler=fsr4   ; the in-game menu owns this line

[FrameGen]
Enabled=false
FGType=fsr3
";

    const LICENSE: &[u8] = b"the license text";

    fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            for (name, data) in files {
                writer
                    .start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }

    /// A fixture release archive (DLL, ini, folder, and a root file that must
    /// not be copied) plus the pin and fetcher that resolve it with no network.
    fn fixture() -> (PinnedRelease, MapFetcher) {
        let archive = zip_bytes(&[
            (RELEASE_DLL, DUMMY_DLL),
            (RELEASE_INI, DUMMY_INI.as_bytes()),
            ("OptiScaler/Licenses/OptiScaler.txt", LICENSE),
            ("README.md", b"release notes, not a game file"),
        ]);
        let digest = sha256_hex(&archive);
        let api = ReleaseSource::GitHub.release_url("optiscaler/OptiScaler", "v0.9.0-test");
        let body = serde_json::json!({
            "tag_name": "v0.9.0-test",
            "assets": [{
                "name": "OptiScaler.zip",
                "browser_download_url": "https://dl.test/OptiScaler.zip",
            }],
        })
        .to_string();
        let fetcher = MapFetcher::new()
            .with(api, body)
            .with("https://dl.test/OptiScaler.zip", archive);
        let pin = PinnedRelease {
            source: ReleaseSource::GitHub,
            project: "optiscaler/OptiScaler".to_string(),
            tag: "v0.9.0-test".to_string(),
            asset: "OptiScaler.zip".to_string(),
            sha256: digest,
        };
        (pin, fetcher)
    }

    /// A game directory holding one ordinary file, as a pre-install tree.
    fn game_tree(root: &Path) {
        fs::write(root.join("game.exe"), b"the game").unwrap();
    }

    fn game(dir: &Path) -> PathBuf {
        let root = dir.join("game");
        fs::create_dir_all(&root).unwrap();
        game_tree(&root);
        root
    }

    // -- DLL choice ---------------------------------------------------------

    #[test]
    fn choose_dll_walks_the_candidate_list_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        assert_eq!(choose_dll(root).unwrap(), DllName::Dxgi);
        fs::write(root.join("dxgi.dll"), b"taken").unwrap();
        assert_eq!(choose_dll(root).unwrap(), DllName::D3d12);
        fs::write(root.join("d3d12.dll"), b"taken").unwrap();
        assert_eq!(choose_dll(root).unwrap(), DllName::Dbghelp);
        fs::write(root.join("dbghelp.dll"), b"taken").unwrap();
        assert_eq!(choose_dll(root).unwrap(), DllName::Winmm);
        fs::write(root.join("winmm.dll"), b"taken").unwrap();
        assert_eq!(choose_dll(root).unwrap(), DllName::Version);
    }

    #[test]
    fn the_override_env_names_the_chosen_dll() {
        assert_eq!(
            override_env(DllName::Winmm),
            vec![("WINEDLLOVERRIDES".to_string(), "winmm=n,b".to_string())]
        );
    }

    // -- exe dir resolution -------------------------------------------------

    #[test]
    fn resolves_the_exe_directory_for_unreal_and_plain_games() {
        let dir = tempfile::tempdir().unwrap();

        // An Unreal shipping exe lives under `*/Binaries/Win64`.
        let win64 = dir.path().join("Gizmo/Binaries/Win64");
        fs::create_dir_all(&win64).unwrap();
        let unreal_exe = win64.join("Gizmo-Win64-Shipping.exe");
        fs::write(&unreal_exe, b"exe").unwrap();
        assert_eq!(resolve_exe_dir(&unreal_exe).unwrap(), win64);
        // The project directory that holds `Binaries/Win64` resolves into it.
        assert_eq!(resolve_exe_dir(&dir.path().join("Gizmo")).unwrap(), win64);
        // And the `Win64` directory itself is left as it is.
        assert_eq!(resolve_exe_dir(&win64).unwrap(), win64);

        // A plain game: the exe's own directory.
        let plain = dir.path().join("Sugar");
        fs::create_dir_all(&plain).unwrap();
        let plain_exe = plain.join("Sugar.exe");
        fs::write(&plain_exe, b"exe").unwrap();
        assert_eq!(resolve_exe_dir(&plain_exe).unwrap(), plain);
        assert_eq!(resolve_exe_dir(&plain).unwrap(), plain);

        // A path that is neither a file nor a directory is refused.
        let error = resolve_exe_dir(&dir.path().join("missing")).unwrap_err();
        assert!(matches!(error, TierBError::TargetMissing { .. }), "{error}");
    }

    // -- deploy -------------------------------------------------------------

    #[test]
    fn deploy_writes_the_dll_ini_folder_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        let (pin, fetcher) = fixture();

        let deployment = deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap();

        assert_eq!(deployment.exe_dir, root);
        assert_eq!(deployment.dll, DllName::Dxgi);
        // The DLL takes the chosen name.
        assert_eq!(fs::read(root.join("dxgi.dll")).unwrap(), DUMMY_DLL);
        // The ini and the release folder are copied; a root file outside the
        // known set is not.
        assert!(root.join(RELEASE_INI).is_file());
        assert_eq!(
            fs::read(root.join("OptiScaler/Licenses/OptiScaler.txt")).unwrap(),
            LICENSE
        );
        assert!(!root.join("README.md").exists());
        // The manifest records the install.
        assert!(ModManifest::is_installed(&root));
        assert_eq!(
            ModManifest::load(&root).unwrap().unwrap().mod_name,
            MOD_NAME
        );
    }

    #[test]
    fn deploy_returns_the_winedll_overrides_as_a_vector() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        let (pin, fetcher) = fixture();

        let deployment = deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap();

        // A vector of (key, value) pairs, never one shell-string.
        assert_eq!(
            deployment.env,
            vec![("WINEDLLOVERRIDES".to_string(), "dxgi=n,b".to_string())]
        );
        let value = &deployment.env[0].1;
        assert!(
            !value.contains(' ') && !value.contains('"'),
            "the override value `{value}` is not a shell string"
        );
    }

    #[test]
    fn deploy_skips_a_dll_name_the_game_owns() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        // The game ships its own dxgi.dll and d3d12.dll: neither must be touched.
        fs::write(root.join("dxgi.dll"), b"the game's own dxgi").unwrap();
        fs::write(root.join("d3d12.dll"), b"another tool's d3d12").unwrap();
        let (pin, fetcher) = fixture();

        let deployment = deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap();

        assert_eq!(deployment.dll, DllName::Dbghelp);
        assert_eq!(
            deployment.env,
            vec![("WINEDLLOVERRIDES".to_string(), "dbghelp=n,b".to_string())]
        );
        // The game's files are untouched; ours landed beside them.
        assert_eq!(
            fs::read(root.join("dxgi.dll")).unwrap(),
            b"the game's own dxgi"
        );
        assert_eq!(
            fs::read(root.join("d3d12.dll")).unwrap(),
            b"another tool's d3d12"
        );
        assert_eq!(fs::read(root.join("dbghelp.dll")).unwrap(), DUMMY_DLL);
    }

    #[test]
    fn deploy_patches_the_ini_line_preservingly() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        let (pin, fetcher) = fixture();

        deploy(
            &root,
            &pin,
            &fetcher,
            staging.path(),
            &[IniSetting::new("FrameGen", "Enabled", "true")],
        )
        .unwrap();

        let ini = fs::read_to_string(root.join(RELEASE_INI)).unwrap();
        assert!(
            ini.contains("Dx12Upscaler=fsr4   ; the in-game menu owns this line"),
            "{ini}"
        );
        assert!(ini.contains("[FrameGen]\nEnabled=true"), "{ini}");
        assert!(ini.contains("FGType=fsr3"), "{ini}");
    }

    #[test]
    fn deploy_refuses_when_every_dll_name_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        for candidate in DllName::CANDIDATES {
            fs::write(root.join(candidate.file_name()), b"taken").unwrap();
        }
        let (pin, fetcher) = fixture();
        let before = tree_hash(&root).unwrap();

        let error = deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap_err();
        assert!(matches!(error, TierBError::NoFreeDll { .. }), "{error}");
        // Nothing was written.
        assert_eq!(tree_hash(&root).unwrap(), before);
        assert!(!ModManifest::is_installed(&root));
    }

    #[test]
    fn a_pin_that_does_not_match_is_refused_without_touching_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        let (mut pin, fetcher) = fixture();
        pin.sha256 = "0".repeat(64);
        let before = tree_hash(&root).unwrap();

        let error = deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap_err();
        assert!(
            matches!(
                error,
                TierBError::Release(ReleaseError::DigestMismatch { .. })
            ),
            "{error}"
        );
        assert_eq!(tree_hash(&root).unwrap(), before);
        assert!(!ModManifest::is_installed(&root));
    }

    #[test]
    fn a_second_deploy_is_refused_until_uninstalled() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        let (pin, fetcher) = fixture();
        deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap();

        let error = deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap_err();
        assert!(
            matches!(error, TierBError::Manifest(ModError::AlreadyInstalled(_))),
            "{error}"
        );
    }

    // -- uninstall ----------------------------------------------------------

    #[test]
    fn uninstall_restores_the_tree_to_its_pre_install_hash() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        let before = tree_hash(&root).unwrap();
        let (pin, fetcher) = fixture();

        deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap();
        assert_ne!(tree_hash(&root).unwrap(), before);

        let report = uninstall(&root).unwrap();
        assert!(report.tree_hash_ok, "{report:?}");
        assert_eq!(report.tree_hash, before);
        assert!(!root.join("dxgi.dll").exists());
        assert!(!root.join(RELEASE_INI).exists());
        assert!(
            !root.join("OptiScaler").exists(),
            "the release folder is pruned"
        );
        assert!(!ModManifest::is_installed(&root));
        assert_eq!(tree_hash(&root).unwrap(), before);
    }

    #[test]
    fn uninstall_leaves_a_dll_the_game_owned_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        fs::write(root.join("dxgi.dll"), b"the game's own dxgi").unwrap();
        let before = tree_hash(&root).unwrap();
        let (pin, fetcher) = fixture();

        let deployment = deploy(&root, &pin, &fetcher, staging.path(), &[]).unwrap();
        assert_eq!(deployment.dll, DllName::D3d12);
        // The game's dxgi.dll was never displaced.
        assert_eq!(
            fs::read(root.join("dxgi.dll")).unwrap(),
            b"the game's own dxgi"
        );

        let report = uninstall(&root).unwrap();
        assert!(report.tree_hash_ok, "{report:?}");
        assert_eq!(
            fs::read(root.join("dxgi.dll")).unwrap(),
            b"the game's own dxgi"
        );
        assert!(!root.join("d3d12.dll").exists());
        assert_eq!(tree_hash(&root).unwrap(), before);
    }

    #[test]
    fn uninstall_without_a_manifest_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = game(dir.path());
        let error = uninstall(&root).unwrap_err();
        assert!(
            matches!(error, TierBError::Manifest(ModError::NoManifest { .. })),
            "{error}"
        );
    }
}
