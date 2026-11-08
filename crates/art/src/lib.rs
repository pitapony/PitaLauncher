//! Art pipeline: download, resize, WebP encode and cache.
//!
//! This crate is framework-free and unaware of Dioxus or SQLite. It manages the
//! on-disk image cache underneath the loopback HTTP server that the webview
//! reads `<img>` tags from.
//!
//! # Cache layout
//!
//! `~/.cache/pitagames/art/<sha256>-<size>.webp` where `<sha256>` is the
//! hex-encoded SHA-256 of the source URL and `<size>` is one of `grid`, `hero`.
//!
//! # Loopback server
//!
//! `http://127.0.0.1:<port>/<token>/<key>` — the random token stops other tabs
//! from reading the cache. The server only accepts `GET` and only serves files
//! whose path component matches a valid cache key.

pub mod cache;
pub mod fetch;
pub mod server;

use std::path::PathBuf;

pub use cache::ArtCache;
pub use fetch::Downloader;
pub use server::ArtServer;

/// The display size an image is prepared for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtSize {
    /// Steam's `capsule_image` (231×87), rendered at 2× on HiDPI.
    Grid,
    /// Steam's `header_image` (460×215), rendered at 2× on HiDPI.
    Hero,
}

/// One image to fetch: a source URL and the size it is prepared for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtRequest {
    pub url: String,
    pub size: ArtSize,
}

impl ArtRequest {
    pub fn new(url: impl Into<String>, size: ArtSize) -> Self {
        Self {
            url: url.into(),
            size,
        }
    }
}

impl ArtSize {
    /// Target pixel width after resize.
    pub fn target_width(self) -> u32 {
        match self {
            Self::Grid => 462,
            Self::Hero => 920,
        }
    }

    /// Target pixel height after resize.
    pub fn target_height(self) -> u32 {
        match self {
            Self::Grid => 174,
            Self::Hero => 430,
        }
    }

    /// Short suffix for the cache file name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Hero => "hero",
        }
    }
}

/// Art pipeline errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("art cache directory does not exist: {0}")]
    NoCacheDir(PathBuf),

    #[error("HTTP download failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("image processing failed: {0}")]
    Image(#[from] image::ImageError),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("loopback server could not bind to {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
