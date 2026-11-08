//! Disk cache: one file per (URL, size) pair, keyed by SHA-256.
//!
//! The cache directory is created on first use so the launcher does not need to
//! pre-create it.

use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{ArtSize, Error, Result};

/// Manages the on-disk art cache.
#[derive(Debug, Clone)]
pub struct ArtCache {
    dir: PathBuf,
}

impl ArtCache {
    /// Point the cache at an existing directory.
    ///
    /// The directory is not created here — the launcher bootstraps it via
    /// `core::paths::cache_dir()` and [`Self::ensure_dir`].
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Create the cache directory when it does not exist yet.
    pub fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(|source| Error::Io {
            path: self.dir.clone(),
            source,
        })
    }

    /// Hex-encoded SHA-256 of the source URL, with the size label appended.
    ///
    /// The full URL (including the query string) is hashed, so a CDN URL that
    /// changes produces a fresh entry instead of serving stale art.
    pub fn key_for(&self, url: &str, size: ArtSize) -> String {
        let hash = Sha256::digest(url.as_bytes());
        format!("{:x}-{}", hash, size.label())
    }

    /// Full path to a cached file, whether or not it exists yet.
    pub fn path_for(&self, url: &str, size: ArtSize) -> PathBuf {
        self.dir
            .join(self.key_for(url, size))
            .with_extension("webp")
    }

    /// Whether the cached file exists on disk.
    pub fn exists(&self, url: &str, size: ArtSize) -> bool {
        self.path_for(url, size).is_file()
    }

    /// Read a cached WebP file into memory.
    pub fn read(&self, url: &str, size: ArtSize) -> Result<Vec<u8>> {
        let path = self.path_for(url, size);
        fs::read(&path).map_err(|source| Error::Io { path, source })
    }

    /// Write bytes to the cache for the given URL and size.
    pub fn write(&self, url: &str, size: ArtSize, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.path_for(url, size);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&path, bytes).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Remove a cached file; no error when it was already absent.
    pub fn remove(&self, url: &str, size: ArtSize) -> Result<()> {
        let path = self.path_for(url, size);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Iterate over the cache directory, yielding (path, file_name) pairs
    /// for every `.webp` file. Missing/inaccessible directories yield an
    /// empty iterator.
    pub fn entries(&self) -> impl Iterator<Item = (PathBuf, String)> {
        let dir = self.dir.clone();
        let read = fs::read_dir(&dir).ok();
        read.into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                (
                    entry.path(),
                    entry.file_name().to_string_lossy().to_string(),
                )
            })
            .filter(|(_, name)| name.ends_with(".webp"))
    }

    /// The root directory of the cache.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_url_same_key() {
        let cache = ArtCache::new(PathBuf::from("/tmp"));
        let key = cache.key_for("https://cdn.example.com/hero.jpg", ArtSize::Grid);
        assert!(key.ends_with("-grid"), "{key}");
        assert_eq!(
            key,
            cache.key_for("https://cdn.example.com/hero.jpg", ArtSize::Grid)
        );
        assert_ne!(
            key,
            cache.key_for("https://cdn.example.com/hero.jpg", ArtSize::Hero)
        );
    }

    #[test]
    fn key_is_64_hex_chars_plus_size_label() {
        let cache = ArtCache::new(PathBuf::from("/tmp"));
        let key = cache.key_for("a", ArtSize::Grid);
        let hex = key.strip_suffix("-grid").unwrap();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn missing_cache_entries_are_silent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().join("nope").join("art"));
        assert!(!cache.exists("https://example.test/img.jpg", ArtSize::Grid));
        assert!(
            cache
                .remove("https://example.test/img.jpg", ArtSize::Grid)
                .is_ok()
        );
        assert!(cache.entries().next().is_none());
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().to_path_buf());
        let url = "https://example.test/capsule.jpg";

        cache.write(url, ArtSize::Grid, b"fake-webp-bytes").unwrap();
        assert!(cache.exists(url, ArtSize::Grid));
        assert_eq!(cache.read(url, ArtSize::Grid).unwrap(), b"fake-webp-bytes");
        assert!(!cache.exists(url, ArtSize::Hero));

        cache.remove(url, ArtSize::Grid).unwrap();
        assert!(!cache.exists(url, ArtSize::Grid));
    }

    #[test]
    fn entries_lists_webp_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().to_path_buf());
        cache.write("url-a", ArtSize::Grid, b"a").unwrap();
        cache.write("url-b", ArtSize::Hero, b"b").unwrap();

        // Drop a non-webp file that should be ignored.
        std::fs::write(dir.path().join("README.txt"), b"hi").unwrap();

        let mut names: Vec<String> = cache.entries().map(|(_, name)| name).collect();
        names.sort();
        assert_eq!(names.len(), 2);
        assert!(names[0].ends_with(".webp"));
        assert!(names[1].ends_with(".webp"));
    }

    #[test]
    fn ensure_dir_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().join("sub").join("art"));
        assert!(!cache.dir.exists());
        cache.ensure_dir().unwrap();
        assert!(cache.dir.is_dir());
    }
}
