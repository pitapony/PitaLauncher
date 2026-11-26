//! Opt-in Proton/GE updates into our own `proton/` directory.
//!
//! PitaGames never patches a Proton build and never writes into Steam's
//! directories. GE-Proton builds are ours to fetch, though: a build is pinned
//! (host, tag, asset, SHA-256) in shipped data, the archive is verified against
//! that pin with [`mods::release`](pitagames_mods::release) before a byte lands,
//! and it is unpacked into `<XDG_DATA_HOME>/pitagames/proton` — a directory
//! [`crate::proton`] already scans, so a build we manage is discovered like any
//! other.
//!
//! ```text
//!  Registry (shipped + user) ──► ProtonPin { tag, asset, sha256 }
//!          │                              │
//!          ├─ status(dir)                 ├─ .pinned() ──► PinnedRelease
//!          └─ available(dir) ──► opt-in ──┴─ apply(pin, dir, fetcher)
//!                                                   │ fetch → verify → unpack
//!                                                   ▼
//!                                    <dir>/GE-Proton11-14-x86_64/proton
//! ```
//!
//! **It is never silent and never automatic.** Nothing here runs on startup or
//! on a schedule: a caller (the CLI today, a Settings screen later) asks for
//! `available`, shows the pinned tag and digest, and calls `apply` only when the
//! user opts in. `apply` returns what it did — the asset, the verified SHA-256
//! and where the build landed — and a mismatch is refused rather than reported
//! as success.
//!
//! **Store CLIs are the package manager's job.** This module manages Proton
//! builds only; it never updates `legendary`/`nile`/`lgogdownloader`/`gogdl`.

use std::fs;
use std::path::{Path, PathBuf};

use pitagames_core::paths;
use pitagames_mods::release::{
    FetchedRelease, Fetcher, LinkPolicy, PinnedRelease, ReleaseError, ReleaseSource,
};
use serde::{Deserialize, Serialize};

use crate::proton::ProtonKind;

/// The shipped pin registry, embedded at build time.
///
/// It is data, not code: the pins are the same "shipped data" ADR-0024 requires
/// for every release fetch, so an update cannot silently become a different
/// build. A user file (see [`default_user_path`]) can add or replace a pin.
pub const SHIPPED: &str = include_str!("../data/proton-updates.toml");

/// One pinned Proton/GE build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtonPin {
    /// The directory the archive extracts to and the build's own name
    /// (`GE-Proton11-14-x86_64`). It is also the directory name under our
    /// `proton/` dir, so `platform::proton` reports it by this name.
    pub name: String,
    /// Which family the build belongs to.
    pub kind: ProtonKind,
    /// The release host.
    pub source: ReleaseSource,
    /// `owner/repo`.
    pub project: String,
    /// The pinned tag.
    pub tag: String,
    /// The exact asset file name to fetch.
    pub asset: String,
    /// The pinned SHA-256 of that asset, lowercase hex.
    pub sha256: String,
}

impl ProtonPin {
    /// The [`PinnedRelease`] this pin resolves to.
    pub fn pinned(&self) -> PinnedRelease {
        PinnedRelease {
            source: self.source,
            project: self.project.clone(),
            tag: self.tag.clone(),
            asset: self.asset.clone(),
            sha256: self.sha256.clone(),
        }
    }
}

/// A set of pinned builds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub builds: Vec<ProtonPin>,
}

impl Registry {
    /// Parse a registry document.
    pub fn parse(text: &str) -> Result<Self, RegistryError> {
        Ok(toml::from_str(text)?)
    }

    /// Read a registry from a file.
    pub fn load(path: &Path) -> Result<Self, RegistryError> {
        let text = fs::read_to_string(path).map_err(|source| RegistryError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }

    /// The shipped registry.
    ///
    /// The embedded file is compiled data, so a malformed one is a build bug; a
    /// test asserts it parses.
    pub fn shipped() -> Self {
        Self::parse(SHIPPED).expect("the shipped Proton pin registry parses")
    }

    /// The shipped registry with a user file merged over it.
    ///
    /// A user pin with the same `name` replaces the shipped one (so a user can
    /// move a build to a newer release without waiting for a PitaGames update);
    /// a new name is added. A missing user file is not an error.
    pub fn effective(user_path: Option<&Path>) -> Result<Self, RegistryError> {
        let mut registry = Self::shipped();
        if let Some(path) = user_path
            && path.is_file()
        {
            registry = registry.merge(Self::load(path)?);
        }
        Ok(registry)
    }

    /// Fold `other` in, `other`'s pin winning on a name collision.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        for pin in other.builds {
            match self
                .builds
                .iter_mut()
                .find(|existing| existing.name == pin.name)
            {
                Some(existing) => *existing = pin,
                None => self.builds.push(pin),
            }
        }
        self
    }

    /// The pin with this name, if any.
    pub fn get(&self, name: &str) -> Option<&ProtonPin> {
        self.builds.iter().find(|pin| pin.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.builds.is_empty()
    }
}

/// `<XDG_CONFIG_HOME>/pitagames/proton/updates.toml`: the user pin override.
pub fn default_user_path() -> Option<PathBuf> {
    paths::config_dir().map(|dir| dir.join("proton").join("updates.toml"))
}

/// The directory the managed builds live in: `<data>/pitagames/proton`.
///
/// This is the same path [`crate::proton::default_roots`] scans as
/// [`crate::proton::ProtonSource::Managed`], so a build unpacked here is picked
/// up by discovery without any further registration.
pub fn default_dir() -> Option<PathBuf> {
    paths::data_dir().map(|dir| dir.join("proton"))
}

/// A pinned build and whether it is already on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildState {
    /// The build's directory name.
    pub name: String,
    pub kind: ProtonKind,
    /// The pinned tag (`GE-Proton11-14`).
    pub tag: String,
    /// `<dir>/<name>`.
    pub path: PathBuf,
    /// `<dir>/<name>/proton` exists, so discovery will find it.
    pub installed: bool,
}

/// The state of every pinned build under `dir`, in pin order.
pub fn status(dir: &Path, registry: &Registry) -> Vec<BuildState> {
    registry
        .builds
        .iter()
        .map(|pin| {
            let path = dir.join(&pin.name);
            BuildState {
                name: pin.name.clone(),
                kind: pin.kind,
                tag: pin.tag.clone(),
                installed: path.join("proton").is_file(),
                path,
            }
        })
        .collect()
}

/// The pinned builds that are not installed yet: the opt-in candidates.
pub fn available(dir: &Path, registry: &Registry) -> Vec<BuildState> {
    status(dir, registry)
        .into_iter()
        .filter(|build| !build.installed)
        .collect()
}

/// What an applied update did, for a log line or a JSON payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateReport {
    pub name: String,
    pub kind: ProtonKind,
    pub tag: String,
    /// The asset that was fetched.
    pub asset: String,
    /// The SHA-256 the bytes actually had (equal to the pin on success).
    pub sha256: String,
    /// Where the build landed (`<dir>/<name>`).
    pub path: PathBuf,
    /// Regular files extracted, as a count.
    pub files: usize,
}

/// Why an update could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum ProtonUpdateError {
    /// The fetch, digest check or unpack failed.
    #[error(transparent)]
    Release(#[from] ReleaseError),

    /// `data_dir()` needs `XDG_DATA_HOME` or `HOME`.
    #[error("no data directory (HOME is unset), so there is nowhere to unpack a build")]
    NoDataDir,

    /// The archive did not unpack a `proton` entrypoint, so it is not a Proton
    /// build (and must not be reported as installed).
    #[error("`{asset}` did not unpack a Proton build at {path} (no `proton` entrypoint)")]
    NotAProtonBuild { asset: String, path: PathBuf },
}

/// Fetch, verify and unpack one pinned build into `dir`.
///
/// Opt-in: nothing calls this on its own. The archive is verified against the
/// pin before anything is written, links are created only when they stay inside
/// `dir` (a Proton build ships its built-in-DLL links as relative symlinks), and
/// the result is confirmed to be a Proton build before it is reported. A failure
/// leaves whatever `mods::release` had already extracted; nothing is deleted, so
/// the next attempt can only overwrite it.
pub fn apply(
    pin: &ProtonPin,
    dir: &Path,
    fetcher: &dyn Fetcher,
) -> Result<UpdateReport, ProtonUpdateError> {
    let FetchedRelease {
        asset,
        sha256,
        files,
        ..
    } = pin
        .pinned()
        .fetch_and_unpack_with(fetcher, dir, LinkPolicy::AllowContained)?;

    let path = dir.join(&pin.name);
    if !path.join("proton").is_file() {
        return Err(ProtonUpdateError::NotAProtonBuild { asset, path });
    }

    Ok(UpdateReport {
        name: pin.name.clone(),
        kind: pin.kind,
        tag: pin.tag.clone(),
        asset,
        sha256,
        path,
        files: files.len(),
    })
}

/// Why a registry could not be read.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the Proton pin registry could not be read: {source}")]
    Toml {
        #[from]
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_mods::release::{MapFetcher, sha256_hex};
    use std::io::Write;

    fn script_proton(dir: &Path, name: &str) {
        let build = dir.join(name);
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("proton"), "#!/usr/bin/env python3\n").unwrap();
    }

    /// A `.tar.gz` with a top-level `<name>/proton`, the GE-Proton shape.
    fn ge_like_archive(name: &str) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let mut builder = tar::Builder::new(Vec::new());
        let proton = b"#!/usr/bin/env python3\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(proton.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{name}/proton"), &proton[..])
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn pin_for(name: &str, archive: &[u8]) -> ProtonPin {
        ProtonPin {
            name: name.to_string(),
            kind: ProtonKind::Ge,
            source: ReleaseSource::GitHub,
            project: "GloriousEggroll/proton-ge-custom".to_string(),
            tag: name.to_string(),
            asset: format!("{name}.tar.gz"),
            sha256: sha256_hex(archive),
        }
    }

    fn fetcher_for(pin: &ProtonPin, archive: Vec<u8>) -> MapFetcher {
        let body = serde_json::json!({
            "tag_name": pin.tag,
            "assets": [{
                "name": pin.asset,
                "browser_download_url": format!("https://dl.test/{}", pin.asset),
            }],
        })
        .to_string();
        MapFetcher::new()
            .with(pin.source.release_url(&pin.project, &pin.tag), body)
            .with(format!("https://dl.test/{}", pin.asset), archive)
    }

    #[test]
    fn the_shipped_registry_parses_and_pins_ge_proton() {
        let registry = Registry::shipped();
        assert!(!registry.is_empty(), "a shipped pin is the point");
        let pin = registry.get("GE-Proton11-14-x86_64").expect("GE pin");
        assert_eq!(pin.kind, ProtonKind::Ge);
        assert_eq!(pin.source, ReleaseSource::GitHub);
        assert_eq!(pin.project, "GloriousEggroll/proton-ge-custom");
        assert_eq!(pin.tag, "GE-Proton11-14");
        assert_eq!(pin.asset, "GE-Proton11-14-x86_64.tar.gz");
        // A 64-hex digest, not a placeholder.
        assert_eq!(pin.sha256.len(), 64);
        assert!(pin.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn a_user_pin_replaces_a_shipped_one_by_name() {
        let shipped = Registry::shipped();
        let name = shipped.builds[0].name.clone();
        let replacement = Registry {
            builds: vec![ProtonPin {
                name: name.clone(),
                sha256: "a".repeat(64),
                ..shipped.builds[0].clone()
            }],
        };

        let merged = Registry::shipped().merge(replacement);
        assert_eq!(merged.get(&name).unwrap().sha256, "a".repeat(64));
        assert_eq!(merged.builds.len(), shipped.builds.len(), "no duplicate");
    }

    #[test]
    fn status_and_available_follow_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let installed = "GE-Proton9-20-x86_64";
        let missing = "GE-Proton11-14-x86_64";
        script_proton(dir.path(), installed);
        let registry = Registry {
            builds: vec![pin_for(installed, b""), pin_for(missing, b"")],
        };

        let states = status(dir.path(), &registry);
        assert!(states[0].installed);
        assert!(!states[1].installed);
        assert_eq!(states[0].path, dir.path().join(installed));

        let available = available(dir.path(), &registry);
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].name, missing);
    }

    #[test]
    fn apply_fetches_verifies_and_unpacks_into_the_managed_dir() {
        let dir = tempfile::tempdir().unwrap();
        let name = "GE-Proton11-14-x86_64";
        let archive = ge_like_archive(name);
        let pin = pin_for(name, &archive);
        let fetcher = fetcher_for(&pin, archive);

        let report = apply(&pin, dir.path(), &fetcher).unwrap();

        assert_eq!(report.name, name);
        assert_eq!(report.asset, format!("{name}.tar.gz"));
        assert_eq!(report.sha256, pin.sha256);
        assert_eq!(report.path, dir.path().join(name));
        assert!(dir.path().join(name).join("proton").is_file());
        // `platform::proton` now discovers it: the directory is a Proton build.
        let install = crate::proton::ProtonInstall::from_dir(
            &report.path,
            crate::proton::ProtonSource::Managed,
        )
        .unwrap();
        assert_eq!(install.kind, ProtonKind::Ge);
    }

    #[test]
    fn a_digest_mismatch_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let name = "GE-Proton11-14-x86_64";
        let archive = ge_like_archive(name);
        let mut pin = pin_for(name, &archive);
        pin.sha256 = "0".repeat(64);
        let fetcher = fetcher_for(&pin, archive);

        let error = apply(&pin, dir.path(), &fetcher).unwrap_err();
        assert!(
            matches!(
                error,
                ProtonUpdateError::Release(ReleaseError::DigestMismatch { .. })
            ),
            "{error}"
        );
        assert!(
            !dir.path().join(name).exists(),
            "a refused fetch must not leave a build behind"
        );
    }

    #[test]
    fn an_archive_that_is_not_a_proton_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let name = "GE-Proton11-14-x86_64";
        // A valid pinned archive, but with no `proton` entrypoint.
        let archive = {
            use flate2::Compression;
            use flate2::write::GzEncoder;

            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{name}/README"), &b"nope"[..])
                .unwrap();
            let tar_bytes = builder.into_inner().unwrap();
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&tar_bytes).unwrap();
            encoder.finish().unwrap()
        };
        let pin = pin_for(name, &archive);
        let fetcher = fetcher_for(&pin, archive);

        let error = apply(&pin, dir.path(), &fetcher).unwrap_err();
        assert!(
            matches!(error, ProtonUpdateError::NotAProtonBuild { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_user_registry_file_overrides_the_shipped_pins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.toml");
        fs::write(
            &path,
            r#"
[[builds]]
name = "GE-Proton12-1-x86_64"
kind = "ge"
source = "github"
project = "GloriousEggroll/proton-ge-custom"
tag = "GE-Proton12-1"
asset = "GE-Proton12-1-x86_64.tar.gz"
sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#,
        )
        .unwrap();

        let registry = Registry::effective(Some(&path)).unwrap();
        assert!(registry.get("GE-Proton12-1-x86_64").is_some());
        // The shipped pin is still there beside it.
        assert!(registry.get("GE-Proton11-14-x86_64").is_some());
    }

    #[test]
    fn a_missing_user_registry_is_not_an_error() {
        let registry = Registry::effective(Some(Path::new("/nowhere/updates.toml"))).unwrap();
        assert_eq!(registry, Registry::shipped());
    }
}
