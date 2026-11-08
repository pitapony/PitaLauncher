//! Download, decode, resize and WebP-encode one image.
//!
//! The pipeline is:
//!   1. HTTP GET the source URL (rustls, 20 s timeout).
//!   2. Decode from whatever format Steam returns (mostly JPEG).
//!   3. Resize to the target 2× dimensions with Lanczos3.
//!   4. Encode as WebP.
//!   5. Write to the disk cache.
//!
//! Art is decorative: every failure is a logged warning, never a reason to
//! block the library. The caller decides whether to retry on the next launch.

use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder};

use crate::cache::ArtCache;
use crate::{ArtRequest, Result};

/// The HTTP side of the art pipeline.
///
/// One client is shared by all downloads so TLS sessions and the connection
/// pool are reused. `ua` is the polite identifier Steam's CDN sees.
#[derive(Debug, Clone)]
pub struct Downloader {
    client: reqwest::Client,
}

impl Downloader {
    /// Build the shared client. The user agent is deliberately plain: this is
    /// a desktop app fetching public store art, not a browser.
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("PitaGames/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self { client })
    }

    /// Download and cache one image when it is not already on disk.
    ///
    /// Returns `true` only when something was actually fetched, so callers can
    /// bump a UI revision and avoid re-rendering for cache hits.
    pub async fn warm(&self, cache: &ArtCache, request: &ArtRequest) -> Result<bool> {
        if cache.exists(&request.url, request.size) {
            return Ok(false);
        }
        self.fetch_and_cache(cache, request).await?;
        Ok(true)
    }

    async fn fetch_and_cache(&self, cache: &ArtCache, request: &ArtRequest) -> Result<()> {
        let bytes = self
            .client
            .get(&request.url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;

        let img = image::load_from_memory(&bytes)?;
        let resized = cover(img, request.size);
        let webp = encode_webp(&resized)?;
        cache.write(&request.url, request.size, &webp)?;
        Ok(())
    }
}

/// Scale to fill the target box, then centre-crop the overflow.
///
/// Steam's capsule and header are both ~2.65:1 / ~2.14:1, but a few apps ship
/// odd aspect ratios. Cropping keeps the art undistorted; `object-fit: cover`
/// still decides what the browser shows.
fn cover(img: DynamicImage, size: crate::ArtSize) -> DynamicImage {
    let target_w = size.target_width();
    let target_h = size.target_height();
    let (width, height) = (img.width().max(1), img.height().max(1));

    let scale = f64::max(
        target_w as f64 / width as f64,
        target_h as f64 / height as f64,
    );
    let scaled_w = ((width as f64 * scale).round() as u32).max(target_w);
    let scaled_h = ((height as f64 * scale).round() as u32).max(target_h);

    let resized = img.resize_exact(scaled_w, scaled_h, FilterType::Lanczos3);
    let offset_x = (scaled_w - target_w) / 2;
    let offset_y = (scaled_h - target_h) / 2;
    resized.crop_imm(offset_x, offset_y, target_w, target_h)
}

/// Lossless WebP: `image-webp` (the backend `image` wraps) only does lossless
/// encoding today. For game art the files are still far smaller than PNG and
/// close to JPEG size at these dimensions.
fn encode_webp(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out).write_image(
        img.as_bytes(),
        img.width(),
        img.height(),
        img.color().into(),
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing real is fetched here — only the encode path is exercised.
    #[test]
    fn a_small_image_encodes_to_webp() {
        let img = DynamicImage::new_rgba8(462, 174);
        let bytes = encode_webp(&img).unwrap();
        assert!(!bytes.is_empty());

        // Verify it is decodable at the requested dimensions.
        let reloaded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(reloaded.width(), 462);
        assert_eq!(reloaded.height(), 174);
    }

    #[test]
    fn hero_is_wider_than_grid() {
        assert!(crate::ArtSize::Hero.target_width() > crate::ArtSize::Grid.target_width());
        assert!(crate::ArtSize::Hero.target_height() > crate::ArtSize::Grid.target_height());
    }

    #[test]
    fn cover_crops_instead_of_stretching() {
        let square = DynamicImage::new_rgba8(100, 100);
        let out = cover(square, crate::ArtSize::Grid);
        assert_eq!((out.width(), out.height()), (462, 174));

        let wide = DynamicImage::new_rgba8(1000, 100);
        let out = cover(wide, crate::ArtSize::Hero);
        assert_eq!((out.width(), out.height()), (920, 430));
    }

    #[test]
    fn warming_a_cached_request_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().to_path_buf());
        let request = ArtRequest::new("https://example.test/a.jpg", crate::ArtSize::Grid);
        cache
            .write(&request.url, request.size, b"already-here")
            .unwrap();

        let downloader = Downloader::new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            !runtime.block_on(downloader.warm(&cache, &request)).unwrap(),
            "a cached image must not be fetched again"
        );
        assert_eq!(
            cache.read(&request.url, request.size).unwrap(),
            b"already-here"
        );
    }
}
