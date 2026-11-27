//! The opt-in support bundle: an explicit allowlist, by construction.
//!
//! When a mod misbehaves, upstream asks for a specific set of files — the mod's
//! own logs, its ini, and the GPU/Mesa/Proton versions (plan §6 item 10). This
//! module collects **exactly that set and nothing else**.
//!
//! The safety is structural, not a filter that could be bypassed: the bundle is
//! built by looking up the exact names in [`ALLOWED_FILES`] inside one game
//! directory, plus an optional Proton log the caller names. There is **no
//! directory walk**, so the NR model weights (`nvngx_dlssnr.dll`, `dlssnr.bin*`)
//! and any secret that happens to share the directory can never enter the bundle
//! — even when they are sitting right beside an included log. The Steam Web API
//! key and a store CLI auth transcript live in the launcher's own trees, which
//! the collector never reads.
//!
//! A test asserts the exclusions by putting every forbidden file beside the
//! allowed ones and reading the written zip back.
//!
//! Framework-free and path-explicit, like [`crate::logs`].

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::gpu::GpuProfile;
use crate::proton::ProtonInstall;

/// The DLSSNR log (the NR engagement marker lives here too).
pub const NR_LOG: &str = "dlssnr-amd.log";
/// The DLSSNR ini (the selective config surface).
pub const NR_INI: &str = "dlssnr-amd.ini";
/// The OptiScaler log.
pub const OPTISCALER_LOG: &str = "OptiScaler.log";
/// The ReShade log.
pub const RESHADE_LOG: &str = "ReShade.log";
/// The HelixSR log.
pub const HELIXSR_LOG: &str = "helixsr.log";

/// The exact file names a bundle may include from a game directory.
///
/// This is the whole allowlist. A name that is not here is never opened, which
/// is why the model weights and every secret are excluded by construction rather
/// than by a deny list that could miss one.
pub const ALLOWED_FILES: [&str; 5] = [NR_LOG, NR_INI, OPTISCALER_LOG, RESHADE_LOG, HELIXSR_LOG];

/// Names that must never appear in a bundle; the test asserts each is absent
/// even when present on disk.
pub const FORBIDDEN_FILES: [&str; 4] = [
    "nvngx_dlssnr.dll",
    "dlssnr.bin",
    "dlssnr-small.bin",
    "steam-api-key",
];

/// The generated versions file's name.
pub const VERSIONS_FILE: &str = "versions.txt";

/// The most bytes one file may contribute; a huge log is tailed, not read whole.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// What a bundle item is, so the manifest can label it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// A mod log from the game directory.
    Log,
    /// The mod's ini from the game directory.
    Ini,
    /// The GPU/Mesa/Proton versions file we generate.
    Versions,
    /// The optional Proton log the caller named.
    ProtonLog,
}

impl ItemKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Ini => "ini",
            Self::Versions => "versions",
            Self::ProtonLog => "proton log",
        }
    }
}

/// One file in a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleItem {
    pub kind: ItemKind,
    /// The name the file is stored under (always a single path component).
    pub name: String,
    /// Where it was read from, when it came off disk.
    pub source: Option<PathBuf>,
    pub bytes: Vec<u8>,
}

/// Everything [`collect`] needs.
///
/// There is deliberately no "logs directory" field: the collector must be unable
/// to reach the launcher's own trees, where the Steam key and an auth transcript
/// live.
#[derive(Debug, Clone)]
pub struct BundleRequest<'a> {
    /// The game directory the mod logs and ini live in.
    pub game_dir: Option<&'a Path>,
    /// The optional Proton log (`PROTON_LOG=1`) the caller names explicitly.
    pub proton_log: Option<&'a Path>,
    /// The rendered GPU/Mesa/Proton versions, from [`versions`].
    pub versions: String,
}

impl<'a> BundleRequest<'a> {
    /// A request with the versions and no files (the versions-only bundle).
    pub fn new(versions: impl Into<String>) -> Self {
        Self {
            game_dir: None,
            proton_log: None,
            versions: versions.into(),
        }
    }

    #[must_use]
    pub fn with_game_dir(mut self, dir: &'a Path) -> Self {
        self.game_dir = Some(dir);
        self
    }

    #[must_use]
    pub fn with_proton_log(mut self, log: &'a Path) -> Self {
        self.proton_log = Some(log);
        self
    }
}

/// A collected support bundle, in memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bundle {
    pub items: Vec<BundleItem>,
    /// Files that were present but could not be read; a bundle never aborts on
    /// one, it says so.
    pub warnings: Vec<String>,
}

impl Bundle {
    /// The stored names, in order.
    pub fn names(&self) -> Vec<&str> {
        self.items.iter().map(|item| item.name.as_str()).collect()
    }

    /// Whether the bundle carries a file under this name.
    pub fn contains(&self, name: &str) -> bool {
        self.items.iter().any(|item| item.name == name)
    }

    /// The bytes stored under a name, if present.
    pub fn read(&self, name: &str) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|item| item.name == name)
            .map(|item| item.bytes.as_slice())
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Write the bundle as a zip archive at `dest`.
    pub fn write_zip(&self, dest: &Path) -> Result<(), SupportError> {
        let file = File::create(dest).map_err(|source| SupportError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for item in &self.items {
            zip.start_file(item.name.clone(), options)
                .map_err(|error| SupportError::Zip {
                    detail: error.to_string(),
                })?;
            zip.write_all(&item.bytes)
                .map_err(|error| SupportError::Zip {
                    detail: error.to_string(),
                })?;
        }
        zip.finish().map_err(|error| SupportError::Zip {
            detail: error.to_string(),
        })?;
        Ok(())
    }
}

/// The suggested file name for a bundle, given a caller-supplied stamp.
pub fn suggested_name(stamp: &str) -> String {
    format!("pitagames-support-{stamp}.zip")
}

/// Collect a bundle: the versions file, the allowlisted game files, then the
/// optional Proton log.
pub fn collect(request: &BundleRequest<'_>) -> Bundle {
    let mut bundle = Bundle::default();

    bundle.items.push(BundleItem {
        kind: ItemKind::Versions,
        name: VERSIONS_FILE.to_string(),
        source: None,
        bytes: request.versions.clone().into_bytes(),
    });

    if let Some(dir) = request.game_dir {
        for &name in &ALLOWED_FILES {
            let path = dir.join(name);
            if !path.is_file() {
                continue;
            }
            match read_capped(&path, MAX_BYTES) {
                Ok(bytes) => bundle.items.push(BundleItem {
                    kind: if name == NR_INI {
                        ItemKind::Ini
                    } else {
                        ItemKind::Log
                    },
                    name: name.to_string(),
                    source: Some(path),
                    bytes,
                }),
                Err(error) => bundle.warnings.push(format!("{name}: {error}")),
            }
        }
    }

    if let Some(path) = request.proton_log
        && path.is_file()
    {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "proton.log".to_string());
        match read_capped(path, MAX_BYTES) {
            Ok(bytes) => bundle.items.push(BundleItem {
                kind: ItemKind::ProtonLog,
                name,
                source: Some(path.to_path_buf()),
                bytes,
            }),
            Err(error) => bundle.warnings.push(format!("{name}: {error}")),
        }
    }

    bundle
}

/// Read at most `cap` bytes; a larger file is tailed and headed with a note.
fn read_capped(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len <= cap {
        return fs::read(path);
    }
    file.seek(SeekFrom::Start(len - cap))?;
    let mut buffer = Vec::with_capacity(cap as usize);
    file.take(cap).read_to_end(&mut buffer)?;
    let mut out = format!("[... {len} bytes; showing the last {cap} ...]\n").into_bytes();
    out.extend_from_slice(&buffer);
    Ok(out)
}

/// The rendered GPU/Mesa/Proton versions the bundle always carries.
///
/// Every unknown is printed as `unknown` rather than guessed, so a bundle from a
/// machine the probe could not read is honest about it.
pub fn versions(gpu: Option<&GpuProfile>, proton: Option<&ProtonInstall>) -> String {
    let mut out = String::new();
    out.push_str("PitaGames support bundle\n");

    match gpu {
        Some(gpu) => {
            let name = gpu.name.as_deref().unwrap_or("unknown model");
            out.push_str(&format!(
                "GPU: {} {name} ({})\n",
                gpu.vendor.label(),
                gpu.arch.label()
            ));
            out.push_str(&format!(
                "Mesa: {}\n",
                gpu.mesa
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ));
            out.push_str(&format!(
                "VRAM: {}\n",
                gpu.vram_bytes
                    .map(|bytes| format!("{bytes} bytes"))
                    .unwrap_or_else(|| "unknown".to_string())
            ));
        }
        None => out.push_str("GPU: unknown\nMesa: unknown\nVRAM: unknown\n"),
    }

    match proton {
        Some(proton) => out.push_str(&format!(
            "Proton: {} {} ({})\n",
            proton.name,
            proton.version.as_deref().unwrap_or("unknown version"),
            proton.path.display()
        )),
        None => out.push_str("Proton: unknown\n"),
    }

    out
}

/// Why a bundle could not be written.
#[derive(Debug, thiserror::Error)]
pub enum SupportError {
    #[error("cannot write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the support bundle zip could not be written: {detail}")]
    Zip { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::Version;
    use crate::gpu::{GpuArch, GpuVendor, HdrSupport};
    use crate::proton::{ProtonKind, ProtonSource};

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn gpu() -> GpuProfile {
        GpuProfile {
            vendor: GpuVendor::Amd,
            arch: GpuArch::Rdna4,
            device_id: None,
            name: Some("AMD Radeon RX 9070 XT".to_string()),
            mesa: Some(Version::parse("26.2.0").unwrap()),
            vram_bytes: Some(16_000_000_000),
            ntsync: false,
            hdr: HdrSupport::Unknown,
            warnings: Vec::new(),
        }
    }

    fn proton() -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/steam/compatibilitytools.d/proton-cachyos"),
            name: "proton-cachyos".to_string(),
            kind: ProtonKind::CachyOs,
            version: Some("proton-cachyos 11.0-20260516".to_string()),
            source: ProtonSource::SystemCompatibilityTools,
        }
    }

    #[test]
    fn versions_render_the_gpu_mesa_and_proton_lines() {
        let text = versions(Some(&gpu()), Some(&proton()));
        assert!(text.contains("AMD Radeon RX 9070 XT"), "{text}");
        assert!(text.contains("RDNA 4"), "{text}");
        assert!(text.contains("Mesa: 26.2"), "{text}");
        assert!(text.contains("proton-cachyos 11.0-20260516"), "{text}");
        assert!(text.contains("/steam/compatibilitytools.d/proton-cachyos"));

        let empty = versions(None, None);
        assert!(empty.contains("GPU: unknown"));
        assert!(empty.contains("Proton: unknown"));
    }

    #[test]
    fn the_allowlisted_files_are_collected_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), NR_LOG, "[NR] NN model loaded\n");
        write(dir.path(), NR_INI, "[Network]\nModel=cc_small_fp8\n");
        write(dir.path(), OPTISCALER_LOG, "OptiScaler\n");
        // Everything below is present but never allowlisted.
        write(dir.path(), "nvngx_dlssnr.dll", "NVIDIA model weights");
        write(dir.path(), "dlssnr.bin", "the weights");
        write(dir.path(), "dlssnr-small.bin", "the small weights");
        write(dir.path(), "steam-api-key", "0123456789abcdef");
        write(dir.path(), "cli-legendary-auth-123.log", "pasted auth code");
        write(dir.path(), "notes.txt", "unrelated");

        let bundle = collect(&BundleRequest::new(versions(None, None)).with_game_dir(dir.path()));
        assert_eq!(
            bundle.names(),
            vec![VERSIONS_FILE, NR_LOG, NR_INI, OPTISCALER_LOG],
            "only the shipped allowlist, in order"
        );
        assert_eq!(bundle.items[1].kind, ItemKind::Log, "an NR log is a log");
        assert_eq!(bundle.items[2].kind, ItemKind::Ini, "the ini is an ini");
    }

    #[test]
    fn the_bundle_never_contains_a_model_or_a_secret_by_construction() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), NR_LOG, "[NR] NN model loaded\n");
        for forbidden in FORBIDDEN_FILES {
            write(dir.path(), forbidden, "THIS MUST NEVER SHIP");
        }
        write(dir.path(), "cli-nile-auth-9.log", "IT MUST NEVER SHIP");

        let bundle = collect(&BundleRequest::new(versions(None, None)).with_game_dir(dir.path()));
        for forbidden in FORBIDDEN_FILES {
            assert!(!bundle.contains(forbidden), "{forbidden} must be excluded");
        }
        assert!(!bundle.contains("cli-nile-auth-9.log"));

        // And the written zip proves it: read every entry back and scan the raw
        // bytes for the forbidden names too.
        let zip_path = dir.path().join("bundle.zip");
        bundle.write_zip(&zip_path).unwrap();
        let bytes = fs::read(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        let mut names = Vec::new();
        for index in 0..archive.len() {
            names.push(archive.by_index(index).unwrap().name().unwrap().to_string());
        }
        assert_eq!(names, vec![VERSIONS_FILE.to_string(), NR_LOG.to_string()]);
        let text = String::from_utf8_lossy(&bytes);
        for forbidden in FORBIDDEN_FILES {
            assert!(
                !text.contains(forbidden),
                "the zip must not carry {forbidden}"
            );
        }
        assert!(!text.contains("MUST NEVER SHIP"));
        assert!(!text.contains("IT MUST NEVER SHIP"));
    }

    #[test]
    fn the_optional_proton_log_is_included_only_when_named() {
        let game = tempfile::tempdir().unwrap();
        write(game.path(), NR_LOG, "log\n");
        let logs = tempfile::tempdir().unwrap();
        let proton_log = write(logs.path(), "steam-620.log", "PROTON LOG\n");

        let without = collect(&BundleRequest::new(versions(None, None)).with_game_dir(game.path()));
        assert!(!without.contains("steam-620.log"));

        let with = collect(
            &BundleRequest::new(versions(None, None))
                .with_game_dir(game.path())
                .with_proton_log(&proton_log),
        );
        assert!(with.contains("steam-620.log"));
        assert_eq!(with.read("steam-620.log"), Some(b"PROTON LOG\n".as_slice()));

        // The other files in the Proton log's directory are still never read.
        write(logs.path(), "cli-nile-auth-9.log", "SECRET");
        assert!(!with.contains("cli-nile-auth-9.log"));
    }

    #[test]
    fn a_missing_game_directory_yields_a_versions_only_bundle() {
        let bundle = collect(&BundleRequest::new("versions\n"));
        assert_eq!(bundle.names(), vec![VERSIONS_FILE]);
        assert!(!bundle.is_empty());
    }

    #[test]
    fn a_large_file_is_tailed_rather_than_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut big = vec![b'a'; (2 * MAX_BYTES) as usize];
        let tail = b"THE END\n";
        big.extend_from_slice(tail);
        fs::write(dir.path().join(NR_LOG), &big).unwrap();

        let bundle = collect(&BundleRequest::new(versions(None, None)).with_game_dir(dir.path()));
        let stored = bundle.read(NR_LOG).unwrap();
        assert!(stored.len() < big.len(), "the file was tailed");
        assert!(stored.ends_with(tail));
        assert!(
            String::from_utf8_lossy(stored).contains("showing the last"),
            "the truncation is stated, not silent"
        );
    }

    #[test]
    fn the_suggested_name_carries_the_stamp() {
        assert_eq!(suggested_name("20261126"), "pitagames-support-20261126.zip");
    }
}
