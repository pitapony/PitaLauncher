//! Pinned release fetching: resolve a release, download one named asset, verify
//! its SHA-256 against a pin *we* ship, and unpack it.
//!
//! The three mod systems PitaGames orchestrates (Tier B OptiScaler, HelixSR,
//! DLSSNR-AMD) each live on a release host — GitLab for the DLSSNR upstream,
//! GitHub for the RDNA 3 fork and HelixSR — and each one must be fetched the
//! same way, because ADR-0024 makes the rules non-negotiable: pin the version,
//! hash-verify every download, and never trust the API's own digest.
//!
//! The seam is the [`Fetcher`] trait: production uses an HTTP client, and every
//! test (and the `pitagames-cli mods release` diagnostic) uses
//! [`MapFetcher`] with fixture bytes, so the whole fetch → verify → unpack
//! pipeline runs with no network.
//!
//! Framework-free and path-explicit: [`PinnedRelease::fetch_and_unpack`] writes
//! into a caller-chosen directory, refuses an archive entry that would escape
//! it, and reports the SHA-256 it actually saw.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Which release host an asset comes from.
///
/// The two shape their release JSON differently, so the source decides both the
/// API URL and how the response is read. Never substitute one for the other:
/// DLSSNR's upstream (GitLab) and the RDNA 3 fork (GitHub) are independent
/// codebases that must be pinned separately (ADR-0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseSource {
    /// `gitlab.com/api/v4/projects/<encoded>/releases/<tag>`.
    GitLab,
    /// `api.github.com/repos/<owner>/<repo>/releases/tags/<tag>`.
    GitHub,
}

impl ReleaseSource {
    /// Both hosts, for an iterating caller (the CLI's help, a test).
    pub const ALL: [ReleaseSource; 2] = [Self::GitLab, Self::GitHub];

    /// A short human label (`GitLab`, `GitHub`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::GitLab => "GitLab",
            Self::GitHub => "GitHub",
        }
    }

    /// The release API URL for `project` at `tag`.
    ///
    /// `project` is `owner/repo` either way; only GitLab needs it
    /// percent-encoded (its API takes the whole path as one segment).
    pub fn release_url(self, project: &str, tag: &str) -> String {
        match self {
            Self::GitHub => format!("https://api.github.com/repos/{project}/releases/tags/{tag}"),
            Self::GitLab => format!(
                "https://gitlab.com/api/v4/projects/{}/releases/{tag}",
                encode_project(project)
            ),
        }
    }

    /// Parse a release API response into host-agnostic [`Release`].
    pub fn parse(self, body: &str) -> Result<Release, ReleaseError> {
        match self {
            Self::GitHub => parse_github(body),
            Self::GitLab => parse_gitlab(body),
        }
    }
}

impl std::fmt::Display for ReleaseSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for ReleaseSource {
    type Err = ReleaseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.to_ascii_lowercase().as_str() {
            "gitlab" => Ok(Self::GitLab),
            "github" => Ok(Self::GitHub),
            other => Err(ReleaseError::UnknownSource(other.to_string())),
        }
    }
}

/// Percent-encode a GitLab project path (`owner/repo` → `owner%2Frepo`).
///
/// Only `/` needs encoding for a project path; the rest of the characters a
/// repository name allows are URL-safe.
fn encode_project(project: &str) -> String {
    project.replace('/', "%2F")
}

/// A release as seen by either host: a tag and the assets it publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The release's tag, e.g. `v0.0.9`.
    pub tag: String,
    /// Its downloadable assets, in the order the host listed them.
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    /// The asset with this exact file name, if the release has one.
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// One downloadable file on a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    /// The asset's file name, e.g. `DLSSNR-AMD-Vulkan-Linux-0.0.9-x86_64.tar.zst`.
    pub name: String,
    /// The URL to download it from.
    pub url: String,
    /// The host's own digest, if it published one. Advisory only — the pin is
    /// what is trusted (ADR-0024).
    pub digest: Option<String>,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

fn parse_github(body: &str) -> Result<Release, ReleaseError> {
    let parsed: GitHubRelease =
        serde_json::from_str(body).map_err(|error| ReleaseError::ReleaseJson {
            host: ReleaseSource::GitHub,
            detail: error.to_string(),
        })?;
    Ok(Release {
        tag: parsed.tag_name,
        assets: parsed
            .assets
            .into_iter()
            .map(|asset| ReleaseAsset {
                name: asset.name,
                url: asset.browser_download_url,
                digest: asset.digest,
            })
            .collect(),
    })
}

#[derive(Deserialize)]
struct GitLabRelease {
    tag_name: String,
    #[serde(default)]
    assets: GitLabAssets,
}

#[derive(Deserialize, Default)]
struct GitLabAssets {
    #[serde(default)]
    links: Vec<GitLabLink>,
}

#[derive(Deserialize)]
struct GitLabLink {
    name: String,
    #[serde(default)]
    direct_asset_url: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

fn parse_gitlab(body: &str) -> Result<Release, ReleaseError> {
    let parsed: GitLabRelease =
        serde_json::from_str(body).map_err(|error| ReleaseError::ReleaseJson {
            host: ReleaseSource::GitLab,
            detail: error.to_string(),
        })?;
    let assets = parsed
        .assets
        .links
        .into_iter()
        .filter_map(|link| {
            // Prefer the public `direct_asset_url`; fall back to `url` for an
            // older response shape. A link with neither is not downloadable.
            let url = link.direct_asset_url.or(link.url)?;
            Some(ReleaseAsset {
                name: link.name,
                url,
                digest: None,
            })
        })
        .collect();
    Ok(Release {
        tag: parsed.tag_name,
        assets,
    })
}

/// A release pinned to one asset and one SHA-256.
///
/// This is the "shipped/caller-provided data" ADR-0024 requires: the version,
/// the exact asset name and the digest are all fixed by us, so a moved tag or a
/// swapped artifact cannot silently become a different install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedRelease {
    /// The host the release lives on.
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

/// What a fetch produced: the bytes, and the digest the bytes actually had.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// The asset name that was fetched.
    pub asset: String,
    /// The SHA-256 the downloaded bytes hash to (equal to the pin on success).
    pub sha256: String,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
}

impl PinnedRelease {
    /// Resolve, download and verify the pinned asset.
    ///
    /// The pin is authoritative: a byte stream whose SHA-256 is not the pinned
    /// value is refused. When the host *also* reported a digest and it disagrees
    /// with the bytes, that is refused too, because a host that contradicts its
    /// own published digest is not something to install from.
    pub fn fetch(&self, fetcher: &dyn Fetcher) -> Result<Fetched, ReleaseError> {
        let body = fetcher
            .get(&self.source.release_url(&self.project, &self.tag))
            .map_err(ReleaseError::Fetch)?;
        let body = String::from_utf8_lossy(&body).into_owned();
        let release = self.source.parse(&body)?;
        let asset = release
            .asset(&self.asset)
            .ok_or_else(|| ReleaseError::AssetMissing {
                project: self.project.clone(),
                tag: self.tag.clone(),
                asset: self.asset.clone(),
            })?
            .clone();
        let bytes = fetcher.get(&asset.url).map_err(ReleaseError::Fetch)?;
        let actual = sha256_hex(&bytes);
        if !hex_eq(&actual, &self.sha256) {
            return Err(ReleaseError::DigestMismatch {
                asset: self.asset.clone(),
                expected: self.sha256.clone(),
                actual,
            });
        }
        if let Some(normalized) = asset.digest.as_deref().and_then(normalize_digest)
            && !hex_eq(&normalized, &actual)
        {
            return Err(ReleaseError::ApiDigestMismatch {
                asset: self.asset.clone(),
                api: normalized,
                actual,
            });
        }
        Ok(Fetched {
            asset: self.asset.clone(),
            sha256: actual,
            bytes,
        })
    }

    /// Fetch the pinned asset and unpack it into `dest`.
    pub fn fetch_and_unpack(
        &self,
        fetcher: &dyn Fetcher,
        dest: &Path,
    ) -> Result<FetchedRelease, ReleaseError> {
        let fetched = self.fetch(fetcher)?;
        let unpacked = unpack_bytes(&fetched.bytes, dest)?;
        Ok(FetchedRelease {
            asset: fetched.asset,
            sha256: fetched.sha256,
            kind: unpacked.kind,
            files: unpacked.files,
            dest: dest.to_path_buf(),
        })
    }
}

/// A release fetched and unpacked: what came down and what landed on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedRelease {
    /// The asset name.
    pub asset: String,
    /// The verified SHA-256.
    pub sha256: String,
    /// The archive format that was detected.
    pub kind: ArchiveKind,
    /// The files extracted, relative to `dest`, forward-slashed and sorted.
    pub files: Vec<String>,
    /// Where they were extracted.
    pub dest: PathBuf,
}

/// How a mod release archive is compressed/packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveKind {
    /// A zip archive (`PK`).
    Zip,
    /// A zstd-compressed tar.
    TarZstd,
    /// An uncompressed tar.
    Tar,
}

impl ArchiveKind {
    /// A short label for a message.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarZstd => "tar.zst",
            Self::Tar => "tar",
        }
    }

    /// Detect the format from the archive's leading bytes.
    ///
    /// Magic bytes, not the file name: the asset name is a host's opinion, the
    /// bytes are what will be read.
    pub fn of(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"PK\x03\x04")
            || bytes.starts_with(b"PK\x05\x06")
            || bytes.starts_with(b"PK\x07\x08")
        {
            return Some(Self::Zip);
        }
        if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            return Some(Self::TarZstd);
        }
        // A ustar magic sits at offset 257 in a tar header.
        if bytes.len() > 262 && &bytes[257..262] == b"ustar" {
            return Some(Self::Tar);
        }
        None
    }
}

/// What [`unpack_bytes`] extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// The detected archive format.
    pub kind: ArchiveKind,
    /// Extracted files, forward-slashed, sorted.
    pub files: Vec<String>,
}

/// Unpack an archive into `dest`, creating it if it does not exist.
///
/// An entry whose path escapes `dest` (`..`, absolute) is refused before it is
/// written, and a symlink or hardlink entry is refused too — a mod release is
/// ordinary files, and following a link out of the game directory is not a
/// thing this will do. The format is chosen by magic bytes:
/// zip, zstd-compressed tar or plain tar.
pub fn unpack_bytes(bytes: &[u8], dest: &Path) -> Result<Unpacked, ReleaseError> {
    let kind = ArchiveKind::of(bytes).ok_or(ReleaseError::UnknownArchive {
        detail: "not a zip, tar.zst or tar archive".to_string(),
    })?;
    fs::create_dir_all(dest).map_err(|error| ReleaseError::Io {
        action: "create",
        path: dest.to_path_buf(),
        source: error,
    })?;
    let mut files = match kind {
        ArchiveKind::Zip => unpack_zip(bytes, dest)?,
        ArchiveKind::TarZstd => {
            let decoder =
                zstd::stream::read::Decoder::new(Cursor::new(bytes)).map_err(|error| {
                    ReleaseError::Unpack {
                        kind: kind.label(),
                        detail: error.to_string(),
                    }
                })?;
            unpack_tar(decoder, dest)?
        }
        ArchiveKind::Tar => unpack_tar(bytes, dest)?,
    };
    files.sort();
    files.dedup();
    Ok(Unpacked { kind, files })
}

fn unpack_zip(bytes: &[u8], dest: &Path) -> Result<Vec<String>, ReleaseError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| ReleaseError::Unpack {
            kind: ArchiveKind::Zip.label(),
            detail: error.to_string(),
        })?;
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ReleaseError::Unpack {
                kind: ArchiveKind::Zip.label(),
                detail: error.to_string(),
            })?;
        if entry.is_dir() {
            continue;
        }
        if !entry.is_file() {
            return Err(ReleaseError::UnsupportedEntry {
                path: entry.name().unwrap_or_default().to_string(),
                kind: "symlink or special file",
            });
        }
        let Some(relative) = entry.enclosed_name() else {
            return Err(ReleaseError::UnsafeEntry(
                entry.name().unwrap_or_default().to_string(),
            ));
        };
        let out = dest.join(&relative);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|error| ReleaseError::Io {
                action: "create",
                path: parent.to_path_buf(),
                source: error,
            })?;
        }
        let mut file = fs::File::create(&out).map_err(|error| ReleaseError::Io {
            action: "write",
            path: out.clone(),
            source: error,
        })?;
        io::copy(&mut entry, &mut file).map_err(|error| ReleaseError::Io {
            action: "write",
            path: out.clone(),
            source: error,
        })?;
        files.push(forward(&relative));
    }
    Ok(files)
}

fn unpack_tar<R: Read>(reader: R, dest: &Path) -> Result<Vec<String>, ReleaseError> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|error| ReleaseError::Unpack {
        kind: ArchiveKind::Tar.label(),
        detail: error.to_string(),
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|error| ReleaseError::Unpack {
            kind: ArchiveKind::Tar.label(),
            detail: error.to_string(),
        })?;
        let path = entry.path().map_err(|error| ReleaseError::Unpack {
            kind: ArchiveKind::Tar.label(),
            detail: error.to_string(),
        })?;
        let path = path.into_owned();
        let entry_type = entry.header().entry_type();
        if !(entry_type.is_file() || entry_type.is_dir()) {
            return Err(ReleaseError::UnsupportedEntry {
                path: path.to_string_lossy().into_owned(),
                kind: "symlink, hardlink or special file",
            });
        }
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|error| ReleaseError::Unpack {
                kind: ArchiveKind::Tar.label(),
                detail: error.to_string(),
            })?;
        if !unpacked {
            return Err(ReleaseError::UnsafeEntry(
                path.to_string_lossy().into_owned(),
            ));
        }
        if entry_type.is_file() {
            files.push(forward(&path));
        }
    }
    Ok(files)
}

/// The forward-slash form of a relative path, for a portable file list.
fn forward(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The SHA-256 of `bytes`, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Normalize an API digest (`sha256:<hex>` or bare hex) to lowercase bare hex,
/// or `None` if it is not a SHA-256.
fn normalize_digest(raw: &str) -> Option<String> {
    let value = raw.strip_prefix("sha256:").unwrap_or(raw).trim();
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(value.to_ascii_lowercase())
    } else {
        None
    }
}

/// Case-insensitive comparison of two hex digests.
fn hex_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

/// A failure reading or verifying a fetched release.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The fetcher could not return a document.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// A release API response was not the JSON this host produces.
    #[error("{host} release response could not be read: {detail}")]
    ReleaseJson {
        /// Which host's response failed.
        host: ReleaseSource,
        /// The serde error.
        detail: String,
    },
    /// The release does not publish the pinned asset.
    #[error("release {tag} of {project} has no asset named `{asset}`")]
    AssetMissing {
        /// `owner/repo`.
        project: String,
        /// The pinned tag.
        tag: String,
        /// The asset that was asked for.
        asset: String,
    },
    /// The downloaded bytes do not hash to the pin.
    #[error("`{asset}` digest mismatch: pinned {expected}, got {actual}")]
    DigestMismatch {
        /// The asset name.
        asset: String,
        /// The pinned SHA-256.
        expected: String,
        /// The SHA-256 the bytes actually had.
        actual: String,
    },
    /// The host's own digest disagrees with the bytes it served.
    #[error("`{asset}` API digest {api} disagrees with the downloaded bytes ({actual})")]
    ApiDigestMismatch {
        /// The asset name.
        asset: String,
        /// The digest the API reported.
        api: String,
        /// The SHA-256 the bytes actually had.
        actual: String,
    },
    /// The archive is not zip, tar.zst or tar.
    #[error("cannot unpack: {detail}")]
    UnknownArchive {
        /// Why it was refused.
        detail: String,
    },
    /// The archive could not be read.
    #[error("cannot unpack the {kind} archive: {detail}")]
    Unpack {
        /// The archive format.
        kind: &'static str,
        /// The reader's error.
        detail: String,
    },
    /// An entry would escape the destination.
    #[error("archive entry `{0}` would escape the destination")]
    UnsafeEntry(String),
    /// An entry is a link or special file this unpack will not create.
    #[error("archive entry `{path}` is a {kind}; refusing to extract it")]
    UnsupportedEntry {
        /// The entry path.
        path: String,
        /// What it is.
        kind: &'static str,
    },
    /// A filesystem operation failed.
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
    /// A release source name was not recognized.
    #[error("unknown release source `{0}` (expected `gitlab` or `github`)")]
    UnknownSource(String),
}

/// A fetcher could not return the requested URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot fetch {url}: {message}")]
pub struct FetchError {
    /// The URL that was requested.
    pub url: String,
    /// Why it failed.
    pub message: String,
}

impl FetchError {
    /// A fetch failure for `url` with a reason.
    pub fn new(url: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            message: message.into(),
        }
    }
}

/// The network seam: given a URL, return its bytes.
///
/// Production plugs in an HTTP client; tests and the CLI diagnostic plug in
/// [`MapFetcher`], so nothing below this trait knows whether the bytes came
/// from the network or a fixture.
pub trait Fetcher {
    /// Return the body at `url`, or a [`FetchError`].
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError>;
}

/// A [`Fetcher`] backed by an in-memory URL → bytes map.
///
/// This is the fixture fetcher: tests and `pitagames-cli mods release` build one
/// from local files so a whole fetch → verify → unpack run can happen offline.
#[derive(Debug, Default, Clone)]
pub struct MapFetcher {
    entries: BTreeMap<String, Vec<u8>>,
}

impl MapFetcher {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a URL → body pair.
    pub fn insert(&mut self, url: impl Into<String>, body: impl Into<Vec<u8>>) {
        self.entries.insert(url.into(), body.into());
    }

    /// Add a URL → body pair, for chaining.
    #[must_use]
    pub fn with(mut self, url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        self.insert(url, body);
        self
    }

    /// The URLs this fetcher can answer.
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

impl Fetcher for MapFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.entries
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::new(url, "no fixture for this URL"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
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

    fn tar_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, *name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn github_release_json(tag: &str, asset: &str, url: &str, digest: Option<&str>) -> String {
        serde_json::json!({
            "tag_name": tag,
            "assets": [{
                "name": asset,
                "browser_download_url": url,
                "digest": digest,
            }],
        })
        .to_string()
    }

    // -- SHA-256 ------------------------------------------------------------

    #[test]
    fn sha256_hex_matches_a_known_vector() {
        // The published SHA-256 of the empty input — an independent known-good
        // value, not recomputed here.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // And of "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // -- release URLs and parsing ------------------------------------------

    #[test]
    fn builds_the_release_url_for_each_host() {
        assert_eq!(
            ReleaseSource::GitHub.release_url("mauri870/DLSSNR-RDNA3", "v0.0.9"),
            "https://api.github.com/repos/mauri870/DLSSNR-RDNA3/releases/tags/v0.0.9"
        );
        // GitLab encodes the project path as one segment.
        assert_eq!(
            ReleaseSource::GitLab.release_url("mochizuki0323/DLSSNR-AMD", "v0.0.9"),
            "https://gitlab.com/api/v4/projects/mochizuki0323%2FDLSSNR-AMD/releases/v0.0.9"
        );
    }

    #[test]
    fn parses_a_github_release_with_its_digest() {
        let body = github_release_json(
            "v1.2.3",
            "OptiScaler.zip",
            "https://example.test/OptiScaler.zip",
            Some("sha256:ab12"),
        );
        let release = ReleaseSource::GitHub.parse(&body).unwrap();
        assert_eq!(release.tag, "v1.2.3");
        let asset = release.asset("OptiScaler.zip").unwrap();
        assert_eq!(asset.url, "https://example.test/OptiScaler.zip");
        assert_eq!(asset.digest.as_deref(), Some("sha256:ab12"));
    }

    #[test]
    fn parses_a_gitlab_release_links() {
        let body = serde_json::json!({
            "tag_name": "v0.0.9",
            "assets": {
                "links": [
                    { "name": "DLSSNR-AMD.tar.zst",
                      "direct_asset_url": "https://gitlab.test/dl" },
                    { "name": "no-url" },
                ],
            },
        })
        .to_string();
        let release = ReleaseSource::GitLab.parse(&body).unwrap();
        assert_eq!(release.tag, "v0.0.9");
        assert_eq!(release.assets.len(), 1, "a link without a URL is dropped");
        assert_eq!(release.assets[0].url, "https://gitlab.test/dl");
    }

    // -- fetch + verify -----------------------------------------------------

    fn pinned(sha256: &str) -> PinnedRelease {
        PinnedRelease {
            source: ReleaseSource::GitHub,
            project: "mauri870/DLSSNR-RDNA3".to_string(),
            tag: "v0.0.9".to_string(),
            asset: "DLSSNR-AMD.zip".to_string(),
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn fetches_a_pinned_asset_and_verifies_its_digest() {
        let archive = zip_bytes(&[("bin/tool", b"the program"), ("readme.txt", b"hello")]);
        let digest = sha256_hex(&archive);
        let api = ReleaseSource::GitHub.release_url("mauri870/DLSSNR-RDNA3", "v0.0.9");
        let body = github_release_json(
            "v0.0.9",
            "DLSSNR-AMD.zip",
            "https://dl.test/DLSSNR-AMD.zip",
            Some(&format!("sha256:{digest}")),
        );
        let fetcher = MapFetcher::new()
            .with(api, body)
            .with("https://dl.test/DLSSNR-AMD.zip", archive.clone());

        let fetched = pinned(&digest).fetch(&fetcher).unwrap();
        assert_eq!(fetched.sha256, digest);
        assert_eq!(fetched.bytes, archive);
    }

    #[test]
    fn a_digest_mismatch_is_refused() {
        let archive = zip_bytes(&[("bin/tool", b"the program")]);
        let api = ReleaseSource::GitHub.release_url("mauri870/DLSSNR-RDNA3", "v0.0.9");
        let body = github_release_json("v0.0.9", "DLSSNR-AMD.zip", "https://dl.test/a.zip", None);
        let fetcher = MapFetcher::new()
            .with(api, body)
            .with("https://dl.test/a.zip", archive);

        // The pin is not the bytes' digest.
        let error = pinned(&"0".repeat(64)).fetch(&fetcher).unwrap_err();
        assert!(
            matches!(error, ReleaseError::DigestMismatch { ref asset, .. } if asset == "DLSSNR-AMD.zip"),
            "{error}"
        );
    }

    #[test]
    fn an_api_digest_that_disagrees_with_the_bytes_is_refused() {
        let archive = zip_bytes(&[("bin/tool", b"the program")]);
        let digest = sha256_hex(&archive);
        let api = ReleaseSource::GitHub.release_url("mauri870/DLSSNR-RDNA3", "v0.0.9");
        // The pin matches the bytes, but the API published a different digest.
        let body = github_release_json(
            "v0.0.9",
            "DLSSNR-AMD.zip",
            "https://dl.test/a.zip",
            Some(&format!("sha256:{}", "a".repeat(64))),
        );
        let fetcher = MapFetcher::new()
            .with(api, body)
            .with("https://dl.test/a.zip", archive);

        let error = pinned(&digest).fetch(&fetcher).unwrap_err();
        assert!(
            matches!(error, ReleaseError::ApiDigestMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_missing_asset_is_named() {
        let api = ReleaseSource::GitHub.release_url("mauri870/DLSSNR-RDNA3", "v0.0.9");
        let body = github_release_json("v0.0.9", "something-else.zip", "https://dl.test/x", None);
        let fetcher = MapFetcher::new().with(api, body);

        let error = pinned(&"a".repeat(64)).fetch(&fetcher).unwrap_err();
        assert!(
            matches!(error, ReleaseError::AssetMissing { ref asset, .. } if asset == "DLSSNR-AMD.zip"),
            "{error}"
        );
    }

    // -- unpacking ----------------------------------------------------------

    #[test]
    fn unpacks_zip_zstd_tar_and_plain_tar() {
        let cases: [(&str, Vec<u8>, ArchiveKind); 3] = [
            (
                "zip",
                zip_bytes(&[("a.txt", b"alpha"), ("dir/b.bin", b"beta")]),
                ArchiveKind::Zip,
            ),
            (
                "tar.zst",
                zstd::encode_all(
                    Cursor::new(tar_bytes(&[("a.txt", b"alpha"), ("dir/b.bin", b"beta")])),
                    3,
                )
                .unwrap(),
                ArchiveKind::TarZstd,
            ),
            (
                "tar",
                tar_bytes(&[("a.txt", b"alpha"), ("dir/b.bin", b"beta")]),
                ArchiveKind::Tar,
            ),
        ];
        for (label, bytes, kind) in cases {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("out");
            let unpacked = unpack_bytes(&bytes, &dest).unwrap();
            assert_eq!(unpacked.kind, kind, "{label}");
            assert_eq!(unpacked.files, ["a.txt", "dir/b.bin"], "{label}");
            assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"alpha", "{label}");
            assert_eq!(
                fs::read(dest.join("dir/b.bin")).unwrap(),
                b"beta",
                "{label}"
            );
        }
    }

    #[test]
    fn fetch_and_unpack_round_trips_a_fixture_archive() {
        let archive =
            zstd::encode_all(Cursor::new(tar_bytes(&[("OptiScaler.dll", b"dll")])), 3).unwrap();
        let digest = sha256_hex(&archive);
        let api = ReleaseSource::GitLab.release_url("mochizuki0323/DLSSNR-AMD", "v0.0.9");
        let body = serde_json::json!({
            "tag_name": "v0.0.9",
            "assets": { "links": [
                { "name": "DLSSNR-AMD.tar.zst", "direct_asset_url": "https://dl.test/d.tar.zst" },
            ]},
        })
        .to_string();
        let fetcher = MapFetcher::new()
            .with(api, body)
            .with("https://dl.test/d.tar.zst", archive);
        let pinned = PinnedRelease {
            source: ReleaseSource::GitLab,
            project: "mochizuki0323/DLSSNR-AMD".to_string(),
            tag: "v0.0.9".to_string(),
            asset: "DLSSNR-AMD.tar.zst".to_string(),
            sha256: digest.clone(),
        };
        let dir = tempfile::tempdir().unwrap();
        let report = pinned
            .fetch_and_unpack(&fetcher, &dir.path().join("mods"))
            .unwrap();
        assert_eq!(report.kind, ArchiveKind::TarZstd);
        assert_eq!(report.files, ["OptiScaler.dll"]);
        assert_eq!(report.sha256, digest);
        assert_eq!(
            fs::read(dir.path().join("mods/OptiScaler.dll")).unwrap(),
            b"dll"
        );
    }

    #[test]
    fn an_unknown_archive_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = unpack_bytes(b"not an archive at all", &dir.path().join("out")).unwrap_err();
        assert!(
            matches!(error, ReleaseError::UnknownArchive { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_zip_entry_that_escapes_the_destination_is_refused() {
        // Build the zip by hand so the entry name keeps its `..` (the writer
        // would sanitize it): a stored entry named `../escape.txt`.
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            writer
                .start_file("../escape.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"nope").unwrap();
            writer.finish().unwrap();
        }
        let bytes = cursor.into_inner();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let error = unpack_bytes(&bytes, &dest).unwrap_err();
        assert!(
            matches!(error, ReleaseError::UnsafeEntry(ref name) if name.contains("escape")),
            "{error}"
        );
        assert!(!dir.path().join("escape.txt").exists());
    }

    #[test]
    fn a_tar_symlink_entry_is_refused() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("/etc/passwd").unwrap();
        header.set_cksum();
        builder
            .append_data(&mut header, "link", io::empty())
            .unwrap();
        let bytes = builder.into_inner().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let error = unpack_bytes(&bytes, &dest).unwrap_err();
        assert!(
            matches!(error, ReleaseError::UnsupportedEntry { .. }),
            "{error}"
        );
    }
}
