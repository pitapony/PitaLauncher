//! The in-app DLSSNR install: the pre-flight form, the model sourcing and the
//! `install.sh` command line.
//!
//! `platform::nr` decides *whether* neural rendering is offered and *which*
//! pinned release backs it. This module is the step after that: turning a
//! person's choices into a command that runs upstream's unmodified `install.sh`
//! in the game's executable directory, and sourcing the one file that is not in
//! the package — the NR model.
//!
//! ```text
//!  InstallForm ──plan()──► InstallPlan { program, args, cwd, cache_dir }
//!       │                        │
//!       │                        └── bash install.sh <exe dir> --dll <path> --model <name>
//!       └── source ──scan()/inspect_model()──► FoundModel { path, version }
//! ```
//!
//! Three rules are structural, not advisory:
//!
//! - **The route is fixed to `optiscaler`.** [`InstallForm::route`] exists so the
//!   form can show it and [`plan`] can refuse anything else, but v1 has exactly
//!   one route (the ReShade feeders are post-v1 and never installed).
//! - **The model is never copied.** The launcher reads the user's own
//!   `nvngx_dlssnr.dll`, verifies its PE version resource, and passes the
//!   *original path* to the installer. [`is_launcher_owned`] refuses a source
//!   that lives inside the launcher's own XDG trees, and the package caches the
//!   extracted weights in its own directory ([`InstallPlan::cache_dir`]) — never
//!   ours (ADR-0024, [`DO_NOT_SHARE`]).
//! - **The installer's own prompts are never guessed.** This module only builds
//!   the command; the prompt policy lives in the runner relay, which surfaces
//!   every question.
//!
//! Framework-free and path-explicit: a `tempfile` tree and a synthetic PE
//! exercise the whole flow with no GPU, no game, no package and no network.

use std::fs;
use std::path::{Path, PathBuf};

use pitagames_core::paths;
use pitagames_core::profile::{
    ModelSource, NeuralRendering, NrBackend, NrModel, NrProcessing, NrRoute,
};

/// The model DLL every DLSSNR build reads from the game directory.
pub const MODEL_DLL_NAME: &str = "nvngx_dlssnr.dll";

/// The version upstream prefers.
pub const PREFERRED_MODEL_VERSION: &str = "310.9.16";
/// The earlier version upstream still supports.
pub const SUPPORTED_MODEL_VERSION: &str = "310.8.0";
/// Both accepted versions, most preferred first.
pub const MODEL_VERSIONS: [&str; 2] = [PREFERRED_MODEL_VERSION, SUPPORTED_MODEL_VERSION];

/// The directory name the DLSSNR package caches its extracted model under.
///
/// It is the package's own directory, never one of ours — the weights are
/// NVIDIA's property and stay where the installer put them.
pub const PACKAGE_CACHE_DIR: &str = "dlssnr-amd";

/// The route v1 ships. The profile's validation refuses any other.
pub const ROUTE: NrRoute = NrRoute::Optiscaler;

/// The sentence the UI must show beside any model path.
pub const DO_NOT_SHARE: &str = "The NR model is NVIDIA's property. Keep it on this machine — do not share, upload or commit it.";

/// The program upstream's install instructions use.
pub const INSTALLER_PROGRAM: &str = "bash";

/// The installer script inside the fetched package.
pub const INSTALLER_SCRIPT: &str = "install.sh";

/// Walk limits for the model scan, so a deep games tree cannot hang the UI.
pub const SCAN_MAX_DEPTH: usize = 6;
/// The number of directory entries a scan may visit before it stops.
pub const SCAN_MAX_ENTRIES: usize = 200_000;

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// A `nvngx_dlssnr.dll` found on this machine, with the version its PE resource
/// reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundModel {
    /// Where the DLL was found.
    pub path: PathBuf,
    /// The `FileVersion` string out of its PE version resource.
    pub version: String,
}

impl FoundModel {
    /// Whether this is upstream's preferred version.
    pub fn is_preferred(&self) -> bool {
        parse_version(&self.version) == parse_version(PREFERRED_MODEL_VERSION)
    }

    /// The [`ModelSource`] a form records for this DLL.
    pub fn source(&self) -> ModelSource {
        ModelSource::ScannedDll {
            path: self.path.clone(),
        }
    }

    /// The label the UI shows, with the preference marker.
    pub fn label(&self) -> String {
        if self.is_preferred() {
            format!("{} ({}, preferred)", self.path.display(), self.version)
        } else {
            format!("{} ({})", self.path.display(), self.version)
        }
    }
}

/// Why a candidate model file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a Windows DLL: {detail}")]
    Binary { path: PathBuf, detail: String },

    #[error("{path} has no version resource")]
    MissingVersion { path: PathBuf },

    #[error(
        "{path} reports version {version}, which is not one DLSSNR accepts (need 310.9.16 or 310.8.0)"
    )]
    UnsupportedVersion { path: PathBuf, version: String },
}

/// Read a DLL's version out of its PE version resource.
///
/// The resource is where a Windows binary records its own `FileVersion`; it is
/// the only trustworthy source, because the file name is whatever the user's
/// game happened to ship. `goblin` parses the `.rsrc` section.
pub fn model_version(path: &Path) -> Result<String, ModelError> {
    let bytes = fs::read(path).map_err(|source| ModelError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let pe = goblin::pe::PE::parse(&bytes).map_err(|error| ModelError::Binary {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    pe.resource_data
        .and_then(|data| data.version_info)
        .and_then(|info| {
            info.string_info
                .file_version()
                .or_else(|| info.string_info.product_version())
        })
        .ok_or_else(|| ModelError::MissingVersion {
            path: path.to_path_buf(),
        })
}

/// Whether a version string names one of the two accepted versions.
///
/// A resource often carries a fourth component (`310.9.16.0`), so only the first
/// three are compared.
pub fn version_supported(version: &str) -> bool {
    let Some(found) = parse_version(version) else {
        return false;
    };
    MODEL_VERSIONS
        .iter()
        .any(|known| parse_version(known) == Some(found))
}

/// Read a user-picked DLL and keep it only if its version is one we accept.
pub fn inspect_model(path: &Path) -> Result<FoundModel, ModelError> {
    let version = model_version(path)?;
    if !version_supported(&version) {
        return Err(ModelError::UnsupportedVersion {
            path: path.to_path_buf(),
            version,
        });
    }
    Ok(FoundModel {
        path: path.to_path_buf(),
        version,
    })
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    let patch = parts.next()?.trim().parse().ok()?;
    Some((major, minor, patch))
}

/// What a scan of the game's directories and Wine prefixes found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelScan {
    /// Every accepted `nvngx_dlssnr.dll`, preferred versions first.
    pub found: Vec<FoundModel>,
    /// A DLL carrying a version we do not accept, with that version.
    pub unsupported: Vec<(PathBuf, String)>,
    /// Files that could not be read or parsed; a scan never aborts on one.
    pub warnings: Vec<String>,
}

impl ModelScan {
    /// Whether nothing usable was found — the cue to offer the file picker.
    pub fn is_empty(&self) -> bool {
        self.found.is_empty()
    }

    /// The model to preselect: a preferred version, else the first found.
    pub fn best(&self) -> Option<&FoundModel> {
        self.found
            .iter()
            .find(|model| model.is_preferred())
            .or_else(|| self.found.first())
    }

    /// Whether the picker should be offered (nothing usable was found).
    pub fn offers_picker(&self) -> bool {
        self.is_empty()
    }
}

/// The roots worth scanning for one game: its install directory and, for a
/// Windows build, its Wine prefix's `drive_c`.
pub fn scan_roots(install_dir: Option<&Path>, prefix: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = install_dir {
        roots.push(dir.to_path_buf());
    }
    if let Some(prefix) = prefix {
        roots.push(prefix.join("drive_c"));
    }
    roots
}

/// Walk `roots` for `nvngx_dlssnr.dll`, verifying each against its PE version.
///
/// Bounded by [`SCAN_MAX_DEPTH`] and [`SCAN_MAX_ENTRIES`]; a symlinked directory
/// is not followed, and a file that cannot be read is a warning, never a stop.
pub fn scan(roots: &[PathBuf]) -> ModelScan {
    let mut scan = ModelScan::default();
    let mut budget = SCAN_MAX_ENTRIES;
    for root in roots {
        walk(root, 0, &mut budget, &mut scan);
    }
    scan.found.sort_by(|a, b| {
        b.is_preferred()
            .cmp(&a.is_preferred())
            .then_with(|| a.path.cmp(&b.path))
    });
    scan.found.dedup_by(|a, b| a.path == b.path);
    scan
}

fn walk(dir: &Path, depth: usize, budget: &mut usize, scan: &mut ModelScan) {
    if depth > SCAN_MAX_DEPTH || *budget == 0 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk(&path, depth + 1, budget, scan);
        } else if path.file_name().is_some_and(|name| name == MODEL_DLL_NAME) {
            match model_version(&path) {
                Ok(version) if version_supported(&version) => {
                    scan.found.push(FoundModel { path, version });
                }
                Ok(version) => scan.unsupported.push((path, version)),
                Err(error) => scan.warnings.push(error.to_string()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The pre-flight form
// ---------------------------------------------------------------------------

/// The pre-flight form: everything a person picks before `install.sh` runs.
///
/// The route is carried (not assumed) so the screen can show it and [`plan`] can
/// refuse a value v1 does not ship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallForm {
    /// Fixed to [`ROUTE`] in v1.
    pub route: NrRoute,
    /// The backend the card is allowed to use.
    pub backend: NrBackend,
    /// The network variant to ask for.
    pub model: NrModel,
    /// The model scale percentage.
    pub model_scale: u8,
    /// The processing style.
    pub processing: NrProcessing,
    /// Where the model comes from.
    pub source: ModelSource,
}

impl InstallForm {
    /// A form with the route fixed and the safe defaults.
    pub fn new(backend: NrBackend, model_scale: u8, source: ModelSource) -> Self {
        Self {
            route: ROUTE,
            backend,
            model: NrModel::CcSmallFp8,
            model_scale,
            processing: NrProcessing::RenoDx,
            source,
        }
    }

    /// The route, which v1 fixes to `optiscaler`.
    pub const fn route(&self) -> NrRoute {
        self.route
    }

    /// Whether the form may be installed: the route must be the one v1 ships.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.route != ROUTE {
            return Err(PlanError::RouteNotAvailable { route: self.route });
        }
        Ok(())
    }

    /// The profile value this form compiles into.
    pub fn neural_rendering(&self, version: impl Into<String>) -> NeuralRendering {
        self.neural_rendering_with_env(version, std::collections::BTreeMap::new())
    }

    /// The profile value this form compiles into, carrying the installer's
    /// captured launch options.
    ///
    /// `captured_env` is what `mods::capture` read from the `%command%` lines
    /// the installer printed (already allowlist-checked); the compiler folds it
    /// into the launch environment. This is how a finished install becomes a
    /// profile without a copy-and-paste step.
    pub fn neural_rendering_with_env(
        &self,
        version: impl Into<String>,
        captured_env: std::collections::BTreeMap<String, String>,
    ) -> NeuralRendering {
        NeuralRendering::DlssNrAmd {
            backend: self.backend,
            version: version.into(),
            route: self.route,
            model: self.model,
            model_scale: self.model_scale,
            processing: self.processing,
            pkg_arch: pitagames_core::profile::PkgArch::X86_64,
            captured_env,
            model_source: self.source.clone(),
        }
    }
}

/// The upstream model name a network variant is asked for by.
///
/// The names are our mapping onto `install.sh --model`; when the launcher does
/// not pass `--model` the installer keeps its own default, so a name upstream
/// renames is a non-fatal mismatch (the prompt is still surfaced for the user).
pub const fn model_arg(model: NrModel) -> &'static str {
    match model {
        NrModel::CcSmallFp8 => "cc_small_fp8",
        NrModel::CcSilverAardwold => "cc_silver_aardwold",
        NrModel::Int4Mixed => "int4_mixed",
        NrModel::SmallFp8Experimental => "small_fp8_experimental",
    }
}

/// Why the launcher will not run an install.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("{path} does not contain the DLSSNR installer script ({script})")]
    MissingInstaller { path: PathBuf, script: &'static str },

    #[error("the model file {path} does not exist")]
    MissingModel { path: PathBuf },

    #[error(
        "{path} is inside the launcher's own directories; the NR model must never be \
         copied there"
    )]
    ModelInLauncherTree { path: PathBuf },

    #[error("route {route:?} is not available in v1 (only optiscaler)")]
    RouteNotAvailable { route: NrRoute },
}

/// Everything `install.sh` needs, composed and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The backend this install is for.
    pub backend: NrBackend,
    /// The program to run (`bash`).
    pub program: String,
    /// The argv, never a shell string.
    pub args: Vec<String>,
    /// The directory the installer runs in: the game's executable directory.
    pub cwd: PathBuf,
    /// The package's own model cache directory (never one of ours).
    pub cache_dir: PathBuf,
    /// Where the model came from.
    pub source: ModelSource,
}

impl InstallPlan {
    /// The label the terminal sheet shows.
    pub fn title(&self, game: &str) -> String {
        format!("DLSSNR — {game}")
    }
}

/// Compose the installer plan, refusing a model that lives in our own trees.
pub fn plan(form: &InstallForm, package_dir: &Path, exe: &Path) -> Result<InstallPlan, PlanError> {
    plan_in(form, package_dir, exe, &launcher_owned_roots())
}

/// [`plan`], with the launcher-owned roots passed in (the seam the tests use).
pub fn plan_in(
    form: &InstallForm,
    package_dir: &Path,
    exe: &Path,
    launcher_owned: &[PathBuf],
) -> Result<InstallPlan, PlanError> {
    form.validate()?;

    let cwd = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| exe.to_path_buf());

    let script = package_dir.join(INSTALLER_SCRIPT);
    if !script.is_file() {
        return Err(PlanError::MissingInstaller {
            path: package_dir.to_path_buf(),
            script: INSTALLER_SCRIPT,
        });
    }

    let mut args = vec![
        script.to_string_lossy().into_owned(),
        cwd.to_string_lossy().into_owned(),
    ];

    match &form.source {
        ModelSource::ScannedDll { path } | ModelSource::UserZip { path } => {
            if is_within(launcher_owned, path) {
                return Err(PlanError::ModelInLauncherTree { path: path.clone() });
            }
            if !path.is_file() {
                return Err(PlanError::MissingModel { path: path.clone() });
            }
            args.push("--dll".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
        // The package already cached the extracted model: a reinstall needs no
        // `--dll` and no file of ours.
        ModelSource::CachedInPackage => {}
    }

    args.push("--model".to_string());
    args.push(model_arg(form.model).to_string());

    Ok(InstallPlan {
        backend: form.backend,
        program: INSTALLER_PROGRAM.to_string(),
        args,
        cwd,
        cache_dir: package_dir.join(PACKAGE_CACHE_DIR),
        source: form.source.clone(),
    })
}

/// The XDG roots the launcher owns.
///
/// A model file inside one of these has been copied into PitaGames' own storage,
/// which is exactly what [`DO_NOT_SHARE`] forbids.
pub fn launcher_owned_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = paths::config_dir() {
        roots.push(dir);
    }
    if let Some(dir) = paths::data_dir() {
        roots.push(dir);
    }
    roots
}

/// The package's own model cache directory for a fetched package.
pub fn package_cache_dir(package_dir: &Path) -> PathBuf {
    package_dir.join(PACKAGE_CACHE_DIR)
}

/// The directory a fetched DLSSNR package unpacks into.
///
/// `<mods root>/dlssnr/<backend>/<version>/`, so each pinned release has its own
/// tree and the package's own model cache sits beside the installer script.
pub fn package_dir(mods_root: &Path, backend: NrBackend, version: &str) -> PathBuf {
    mods_root
        .join("dlssnr")
        .join(crate::nr::backend_label(backend))
        .join(version)
}

/// Whether `path` is inside any of `roots`.
pub fn is_within(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Whether a model path lives inside the launcher's own XDG trees.
pub fn is_launcher_owned(path: &Path) -> bool {
    is_within(&launcher_owned_roots(), path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::profile::PkgArch;

    // -- a synthetic PE32+ carrying a VS_VERSIONINFO resource ---------------

    fn utf16z(text: &str) -> Vec<u8> {
        let mut out: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn pad4(out: &mut Vec<u8>) {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    /// One `VS_VERSIONINFO` block, rendered the way the resource compiler lays
    /// them out (key padded to 4, value, then children).
    struct Block {
        value_len_words: u16,
        ty: u16,
        key: String,
        value: Vec<u8>,
        children: Vec<Block>,
    }

    impl Block {
        fn text(key: &str, value: &str) -> Self {
            Self {
                value_len_words: value.encode_utf16().count() as u16 + 1,
                ty: 1,
                key: key.to_string(),
                value: utf16z(value),
                children: Vec::new(),
            }
        }

        fn group(key: &str, children: Vec<Block>) -> Self {
            Self {
                value_len_words: 0,
                ty: 1,
                key: key.to_string(),
                value: Vec::new(),
                children,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&0u16.to_le_bytes()); // wLength, patched below
            out.extend_from_slice(&self.value_len_words.to_le_bytes());
            out.extend_from_slice(&self.ty.to_le_bytes());
            out.extend_from_slice(&utf16z(&self.key));
            pad4(&mut out);
            out.extend_from_slice(&self.value);
            pad4(&mut out);
            for child in &self.children {
                out.extend_from_slice(&child.bytes());
            }
            let len = out.len() as u16;
            out[0..2].copy_from_slice(&len.to_le_bytes());
            out
        }
    }

    /// The `RT_VERSION` data: `VS_VERSION_INFO` with a `StringFileInfo` table
    /// carrying `FileVersion` and `ProductVersion`.
    fn version_data(version: &str) -> Vec<u8> {
        let (major, minor, patch) = parse_version(version).unwrap();
        let mut fixed = vec![0u8; 52];
        fixed[0..4].copy_from_slice(&0xFEEF04BDu32.to_le_bytes());
        fixed[4..8].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        fixed[8..12].copy_from_slice(&((major << 16) | minor).to_le_bytes());
        fixed[12..16].copy_from_slice(&(patch << 16).to_le_bytes());

        Block {
            value_len_words: 52, // a binary value, so this is a byte count
            ty: 0,
            key: "VS_VERSION_INFO".to_string(),
            value: fixed,
            children: vec![Block::group(
                "StringFileInfo",
                vec![Block::group(
                    "040904B0",
                    vec![
                        Block::text("FileVersion", version),
                        Block::text("ProductVersion", version),
                    ],
                )],
            )],
        }
        .bytes()
    }

    fn align_up(value: usize, align: usize) -> usize {
        value.div_ceil(align) * align
    }

    /// A minimal but valid PE32+ whose `.rsrc` section holds one `RT_VERSION`.
    fn versioned_dll(version: &str) -> Vec<u8> {
        let version_bytes = version_data(version);
        let rsrc_rva: u32 = 0x1000;
        let raw_ptr: usize = 0x200;

        let mut rsrc = vec![0u8; 0x58];
        // Root directory: one ID entry.
        rsrc[14..16].copy_from_slice(&1u16.to_le_bytes());
        rsrc[0x10..0x14].copy_from_slice(&16u32.to_le_bytes()); // RT_VERSION
        rsrc[0x14..0x18].copy_from_slice(&(0x8000_0000u32 | 0x18).to_le_bytes());
        // Subdirectory (type 16): one ID entry -> id 1.
        rsrc[0x18 + 14..0x18 + 16].copy_from_slice(&1u16.to_le_bytes());
        rsrc[0x28..0x2C].copy_from_slice(&1u32.to_le_bytes());
        rsrc[0x2C..0x30].copy_from_slice(&(0x8000_0000u32 | 0x30).to_le_bytes());
        // Subdirectory (id 1): one ID entry -> language 0x409, which is data.
        rsrc[0x30 + 14..0x30 + 16].copy_from_slice(&1u16.to_le_bytes());
        rsrc[0x40..0x44].copy_from_slice(&0x409u32.to_le_bytes());
        rsrc[0x44..0x48].copy_from_slice(&0x48u32.to_le_bytes());
        // ResourceDataEntry -> the version block.
        let version_rva = rsrc_rva + 0x58;
        rsrc[0x48..0x4C].copy_from_slice(&version_rva.to_le_bytes());
        rsrc[0x4C..0x50].copy_from_slice(&(version_bytes.len() as u32).to_le_bytes());
        rsrc.extend_from_slice(&version_bytes);

        let rsrc_size = rsrc.len();
        let raw_size = align_up(rsrc_size, 0x200);

        let mut file = vec![0u8; raw_ptr + raw_size];
        file[0..2].copy_from_slice(b"MZ");
        file[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        file[0x40..0x44].copy_from_slice(b"PE\0\0");
        // COFF header.
        file[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes()); // AMD64
        file[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // one section
        file[0x54..0x56].copy_from_slice(&0xF0u16.to_le_bytes()); // optional header size
        file[0x56..0x58].copy_from_slice(&0x0022u16.to_le_bytes()); // executable
        // PE32+ optional header.
        file[0x58..0x5A].copy_from_slice(&0x20Bu16.to_le_bytes());
        file[0x78..0x7C].copy_from_slice(&0x1000u32.to_le_bytes()); // SectionAlignment
        file[0x7C..0x80].copy_from_slice(&0x200u32.to_le_bytes()); // FileAlignment
        file[0x90..0x94].copy_from_slice(&0x2000u32.to_le_bytes()); // SizeOfImage
        file[0x94..0x98].copy_from_slice(&0x200u32.to_le_bytes()); // SizeOfHeaders
        file[0xC4..0xC8].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
        // Data directory index 2 (resource table).
        file[0xD8..0xDC].copy_from_slice(&rsrc_rva.to_le_bytes());
        file[0xDC..0xE0].copy_from_slice(&(rsrc_size as u32).to_le_bytes());
        // One section header: `.rsrc`.
        let s = 0x148;
        file[s..s + 8].copy_from_slice(b".rsrc\0\0\0");
        file[s + 8..s + 12].copy_from_slice(&(rsrc_size as u32).to_le_bytes());
        file[s + 12..s + 16].copy_from_slice(&rsrc_rva.to_le_bytes());
        file[s + 16..s + 20].copy_from_slice(&(raw_size as u32).to_le_bytes());
        file[s + 20..s + 24].copy_from_slice(&(raw_ptr as u32).to_le_bytes());
        file[s + 36..s + 40].copy_from_slice(&0x4000_0040u32.to_le_bytes());
        file[raw_ptr..raw_ptr + rsrc_size].copy_from_slice(&rsrc);
        file
    }

    fn write_dll(dir: &Path, name: &str, version: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, versioned_dll(version)).unwrap();
        path
    }

    fn write_installer(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let script = dir.join(INSTALLER_SCRIPT);
        fs::write(&script, "#!/bin/sh\n").unwrap();
        dir.to_path_buf()
    }

    fn write_exe(dir: &Path) -> PathBuf {
        let exe = dir.join("Game/Binaries/Win64/Game.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"MZ").unwrap();
        exe
    }

    // -- reading the model version ------------------------------------------

    #[test]
    fn the_pe_version_resource_is_read_and_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let dll = write_dll(dir.path(), MODEL_DLL_NAME, PREFERRED_MODEL_VERSION);

        assert_eq!(model_version(&dll).unwrap(), PREFERRED_MODEL_VERSION);
        assert!(version_supported(PREFERRED_MODEL_VERSION));
        assert!(version_supported(SUPPORTED_MODEL_VERSION));
        // A resource often carries a fourth component.
        assert!(version_supported("310.9.16.0"));
        assert!(!version_supported("300.1.0"));
        assert!(!version_supported("not a version"));

        let found = inspect_model(&dll).unwrap();
        assert!(found.is_preferred());
        assert!(found.label().contains("preferred"));
    }

    #[test]
    fn an_unsupported_version_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let dll = write_dll(dir.path(), MODEL_DLL_NAME, "300.1.0");
        let error = inspect_model(&dll).unwrap_err();
        assert!(matches!(error, ModelError::UnsupportedVersion { .. }));
        assert!(error.to_string().contains("300.1.0"));
    }

    #[test]
    fn a_file_that_is_not_a_pe_has_no_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MODEL_DLL_NAME);
        fs::write(&path, b"not a dll").unwrap();
        assert!(matches!(
            model_version(&path),
            Err(ModelError::Binary { .. })
        ));
    }

    // -- sourcing -----------------------------------------------------------

    #[test]
    fn sourcing_finds_a_fixture_dll_and_otherwise_offers_the_picker() {
        let dir = tempfile::tempdir().unwrap();

        // Nothing there yet: the scan is empty and the picker is offered.
        let empty = scan(&[dir.path().to_path_buf()]);
        assert!(empty.is_empty());
        assert!(empty.offers_picker());
        assert!(empty.best().is_none());

        // A game subtree holding the DLL is found, preferred version first.
        let game = dir.path().join("Game/Binaries/Win64");
        fs::create_dir_all(&game).unwrap();
        write_dll(&game, MODEL_DLL_NAME, PREFERRED_MODEL_VERSION);

        let found = scan(&[dir.path().to_path_buf()]);
        assert_eq!(found.found.len(), 1);
        assert!(found.best().unwrap().is_preferred());
        assert!(!found.offers_picker());
        assert_eq!(
            found.best().unwrap().source(),
            ModelSource::ScannedDll {
                path: game.join(MODEL_DLL_NAME)
            }
        );
    }

    #[test]
    fn a_prefix_scan_looks_under_drive_c() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("pfx");
        let drive_c = prefix.join("drive_c/Program Files/Game");
        fs::create_dir_all(&drive_c).unwrap();
        write_dll(&drive_c, MODEL_DLL_NAME, SUPPORTED_MODEL_VERSION);

        let roots = scan_roots(None, Some(&prefix));
        assert_eq!(roots, vec![prefix.join("drive_c")]);

        let found = scan(&roots);
        assert_eq!(found.found.len(), 1);
        assert!(!found.best().unwrap().is_preferred());
    }

    #[test]
    fn a_scan_reports_an_unsupported_dll_without_accepting_it() {
        let dir = tempfile::tempdir().unwrap();
        write_dll(dir.path(), MODEL_DLL_NAME, "300.1.0");

        let scan = scan(&[dir.path().to_path_buf()]);
        assert!(scan.found.is_empty());
        assert_eq!(scan.unsupported.len(), 1);
        assert_eq!(scan.unsupported[0].1, "300.1.0");
    }

    // -- the plan -----------------------------------------------------------

    #[test]
    fn the_plan_composes_the_installer_command_with_the_exe_dir_as_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let package = write_installer(&dir.path().join("pkg"));
        let exe = write_exe(dir.path());
        let model = write_dll(dir.path(), MODEL_DLL_NAME, PREFERRED_MODEL_VERSION);

        let form = InstallForm::new(
            NrBackend::Rdna4Upstream,
            100,
            ModelSource::ScannedDll {
                path: model.clone(),
            },
        );
        let plan = plan(&form, &package, &exe).unwrap();

        assert_eq!(plan.program, INSTALLER_PROGRAM);
        assert_eq!(plan.cwd, exe.parent().unwrap());
        assert_eq!(
            plan.args[0],
            package.join(INSTALLER_SCRIPT).to_string_lossy()
        );
        assert_eq!(plan.args[1], plan.cwd.to_string_lossy());
        assert!(plan.args.contains(&"--dll".to_string()));
        assert!(plan.args.contains(&model.to_string_lossy().into_owned()));
        assert!(plan.args.contains(&"--model".to_string()));
        assert!(plan.args.contains(&"cc_small_fp8".to_string()));
        assert_eq!(plan.cache_dir, package.join(PACKAGE_CACHE_DIR));
    }

    #[test]
    fn a_cached_model_needs_no_dll_argument() {
        let dir = tempfile::tempdir().unwrap();
        let package = write_installer(&dir.path().join("pkg"));
        let exe = write_exe(dir.path());

        let form = InstallForm::new(NrBackend::Rdna3Fork, 50, ModelSource::CachedInPackage);
        let plan = plan(&form, &package, &exe).unwrap();
        assert!(!plan.args.contains(&"--dll".to_string()));
        assert!(plan.cache_dir.ends_with(PACKAGE_CACHE_DIR));
    }

    #[test]
    fn a_missing_installer_script_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("empty-pkg");
        fs::create_dir_all(&package).unwrap();
        let exe = write_exe(dir.path());

        let form = InstallForm::new(NrBackend::Rdna4Upstream, 100, ModelSource::CachedInPackage);
        let error = plan(&form, &package, &exe).unwrap_err();
        assert!(matches!(error, PlanError::MissingInstaller { .. }));
        assert!(error.to_string().contains(INSTALLER_SCRIPT));
    }

    #[test]
    fn the_route_is_fixed_to_optiscaler() {
        let mut form =
            InstallForm::new(NrBackend::Rdna4Upstream, 100, ModelSource::CachedInPackage);
        assert_eq!(form.route(), ROUTE);
        assert!(form.validate().is_ok());

        form.route = NrRoute::ReshadeFeeder;
        assert!(matches!(
            form.validate(),
            Err(PlanError::RouteNotAvailable { .. })
        ));
    }

    #[test]
    fn a_model_inside_the_launcher_trees_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let owned = dir.path().join("xdg/pitagames");
        let model = owned.join("models").join(MODEL_DLL_NAME);
        fs::create_dir_all(model.parent().unwrap()).unwrap();
        fs::write(&model, versioned_dll(PREFERRED_MODEL_VERSION)).unwrap();

        let package = write_installer(&dir.path().join("pkg"));
        let exe = write_exe(dir.path());
        let form = InstallForm::new(
            NrBackend::Rdna4Upstream,
            100,
            ModelSource::ScannedDll {
                path: model.clone(),
            },
        );

        let error = plan_in(&form, &package, &exe, std::slice::from_ref(&owned)).unwrap_err();
        assert!(matches!(error, PlanError::ModelInLauncherTree { .. }));
        assert!(error.to_string().contains("launcher's own"));
    }

    #[test]
    fn a_missing_model_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let package = write_installer(&dir.path().join("pkg"));
        let exe = write_exe(dir.path());
        let form = InstallForm::new(
            NrBackend::Rdna4Upstream,
            100,
            ModelSource::UserZip {
                path: dir.path().join("picked.zip"),
            },
        );
        assert!(matches!(
            plan(&form, &package, &exe),
            Err(PlanError::MissingModel { .. })
        ));
    }

    #[test]
    fn is_launcher_owned_recognises_the_pitagames_trees() {
        if let Some(data) = paths::data_dir() {
            assert!(is_launcher_owned(
                &data.join("mods/dlssnr/x").join(MODEL_DLL_NAME)
            ));
        }
        if let Some(config) = paths::config_dir() {
            assert!(is_launcher_owned(
                &config.join("dlssnr").join(MODEL_DLL_NAME)
            ));
        }
        assert!(!is_launcher_owned(
            Path::new("/games/Steam/steamapps/common/Game")
                .join(MODEL_DLL_NAME)
                .as_path()
        ));
    }

    #[test]
    fn the_form_compiles_to_a_neural_rendering_profile_value() {
        let form = InstallForm::new(NrBackend::Rdna4Upstream, 100, ModelSource::CachedInPackage);
        let nr = form.neural_rendering("0.0.9");
        assert!(nr.is_enabled());
        match nr {
            NeuralRendering::DlssNrAmd {
                route,
                backend,
                model_scale,
                pkg_arch,
                ..
            } => {
                assert_eq!(route, ROUTE);
                assert_eq!(backend, NrBackend::Rdna4Upstream);
                assert_eq!(model_scale, 100);
                assert_eq!(pkg_arch, PkgArch::X86_64);
            }
            NeuralRendering::Off => panic!("expected an enabled profile value"),
        }
    }

    #[test]
    fn the_form_carries_the_installers_captured_env() {
        let form = InstallForm::new(NrBackend::Rdna4Upstream, 75, ModelSource::CachedInPackage);
        let env = std::collections::BTreeMap::from([(
            "WINEDLLOVERRIDES".to_string(),
            "dxgi=n,b".to_string(),
        )]);
        let nr = form.neural_rendering_with_env("0.0.9", env.clone());
        match nr {
            NeuralRendering::DlssNrAmd {
                version,
                model_scale,
                captured_env,
                ..
            } => {
                assert_eq!(version, "0.0.9");
                assert_eq!(model_scale, 75);
                assert_eq!(captured_env, env);
            }
            NeuralRendering::Off => panic!("expected an enabled profile value"),
        }
    }

    #[test]
    fn the_do_not_share_rule_is_stated() {
        assert!(DO_NOT_SHARE.contains("do not share"));
        assert!(DO_NOT_SHARE.contains("NVIDIA"));
    }

    #[test]
    fn the_package_dir_names_the_backend_and_version() {
        let dir = package_dir(Path::new("/data/mods"), NrBackend::Rdna4Upstream, "0.0.9");
        assert_eq!(dir, PathBuf::from("/data/mods/dlssnr/rdna4_upstream/0.0.9"));
        let cache = package_cache_dir(&dir);
        assert_eq!(cache, dir.join(PACKAGE_CACHE_DIR));
    }
}
