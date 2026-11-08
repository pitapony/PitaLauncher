//! Art wiring for the UI: cache directory, loopback server, downloads.
//!
//! The renderer only ever sees a URL when the image is already on disk, so a
//! card never points the webview at a file that does not exist yet. Downloads
//! happen in the background and bump `art_revision` in `main.rs`, which makes
//! the grid and detail page re-render with the new `<img>` sources.

use std::sync::Arc;

use pitagames_art::{ArtCache, ArtRequest, ArtServer, ArtSize, Downloader};
use pitagames_core::GameMetadata;
use pitagames_db::StoredMetadata;

/// Everything the art pipeline needs, created once per window.
pub struct ArtContext {
    cache: ArtCache,
    server: ArtServer,
    downloader: Downloader,
}

impl ArtContext {
    /// Create the cache directory, bind the loopback server and build the HTTP
    /// client. Failures are display-ready strings: the launcher keeps working
    /// without art. Async because the axum listener binds through tokio.
    pub async fn start() -> Result<Arc<Self>, String> {
        let cache_dir = pitagames_core::paths::cache_dir()
            .ok_or_else(|| "Art is unavailable: HOME is not set".to_string())?
            .join("art");
        let cache = ArtCache::new(cache_dir);
        cache
            .ensure_dir()
            .map_err(|error| format!("Art cache is unavailable: {error}"))?;

        let server = ArtServer::start(cache.clone())
            .await
            .map_err(|error| format!("Art server did not start: {error}"))?;
        let downloader =
            Downloader::new().map_err(|error| format!("Art downloads are unavailable: {error}"))?;

        Ok(Arc::new(Self {
            cache,
            server,
            downloader,
        }))
    }

    /// Loopback URL for an image that is already cached, `None` otherwise.
    pub fn url(&self, url: Option<&str>, size: ArtSize) -> Option<String> {
        let url = url?;
        if !self.cache.exists(url, size) {
            return None;
        }
        Some(self.server.image_url(&self.cache, url, size))
    }

    /// First URL in `candidates` that has a cached image at `size`.
    ///
    /// Lets the UI fall back without knowing which URL the store actually
    /// returned: capsule → header for the grid, header → background for the
    /// hero.
    pub fn url_for(&self, candidates: &[Option<&str>], size: ArtSize) -> Option<String> {
        candidates.iter().find_map(|url| self.url(*url, size))
    }

    /// Cache one image. `true` means it was downloaded (the UI should
    /// re-render); failures are logged and treated as "no art".
    pub async fn warm(&self, request: &ArtRequest) -> bool {
        match self.downloader.warm(&self.cache, request).await {
            Ok(downloaded) => downloaded,
            Err(error) => {
                eprintln!("[pitagames] art {}: {error}", request.url);
                false
            }
        }
    }
}

/// The images one game wants: a grid capsule and a detail hero.
///
/// Steam does not always return every variant, so each size falls back to the
/// closest available URL instead of leaving a hole.
pub fn requests_for(metadata: &GameMetadata) -> Vec<ArtRequest> {
    let mut requests = Vec::new();

    let grid = metadata
        .capsule_image
        .as_deref()
        .or(metadata.header_image.as_deref());
    if let Some(url) = grid {
        requests.push(ArtRequest::new(url, ArtSize::Grid));
    }

    let hero = metadata
        .header_image
        .as_deref()
        .or(metadata.background.as_deref());
    if let Some(url) = hero {
        requests.push(ArtRequest::new(url, ArtSize::Hero));
    }

    requests
}

/// Art requests for a batch of freshly cached metadata rows.
///
/// Negative rows (the store has no data) contribute nothing.
pub fn requests_for_rows(rows: &[StoredMetadata]) -> Vec<ArtRequest> {
    let mut requests: Vec<ArtRequest> = Vec::new();
    for row in rows {
        let Some(metadata) = row.metadata.as_ref() else {
            continue;
        };
        for request in requests_for(metadata) {
            if !requests.contains(&request) {
                requests.push(request);
            }
        }
    }
    requests
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{GameId, Store};

    fn game() -> GameMetadata {
        GameMetadata::new(GameId::new(Store::Steam, "620"), "Portal 2")
    }

    #[test]
    fn capsule_is_grid_and_header_is_hero() {
        let mut metadata = game();
        metadata.capsule_image = Some("capsule.jpg".into());
        metadata.header_image = Some("header.jpg".into());
        metadata.background = Some("background.jpg".into());

        let requests = requests_for(&metadata);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], ArtRequest::new("capsule.jpg", ArtSize::Grid));
        assert_eq!(requests[1], ArtRequest::new("header.jpg", ArtSize::Hero));
    }

    #[test]
    fn missing_capsule_falls_back_to_header_for_grid() {
        let mut metadata = game();
        metadata.header_image = Some("header.jpg".into());
        metadata.background = Some("background.jpg".into());

        let requests = requests_for(&metadata);
        assert_eq!(
            requests,
            [
                ArtRequest::new("header.jpg", ArtSize::Grid),
                ArtRequest::new("header.jpg", ArtSize::Hero),
            ]
        );
    }

    #[test]
    fn missing_header_falls_back_to_background_for_the_hero() {
        let mut metadata = game();
        metadata.background = Some("background.jpg".into());

        // The grid stays empty: a 1920×620 backdrop is not worth fetching just
        // to crop it into a 231×87 capsule.
        assert_eq!(
            requests_for(&metadata),
            [ArtRequest::new("background.jpg", ArtSize::Hero)]
        );
    }

    #[test]
    fn a_game_without_art_wants_nothing() {
        assert!(requests_for(&game()).is_empty());
    }

    #[test]
    fn rows_are_deduplicated_and_skip_negatives() {
        let mut metadata = game();
        metadata.capsule_image = Some("capsule.jpg".into());
        metadata.header_image = Some("header.jpg".into());

        let rows = vec![
            StoredMetadata {
                game: metadata.game.clone(),
                metadata: Some(metadata.clone()),
                fetched_at: time::OffsetDateTime::now_utc(),
            },
            StoredMetadata {
                game: metadata.game.clone(),
                // A duplicate row (same game seen twice) must not double-fetch.
                metadata: Some(metadata),
                fetched_at: time::OffsetDateTime::now_utc(),
            },
            StoredMetadata {
                game: GameId::new(Store::Steam, "999"),
                metadata: None,
                fetched_at: time::OffsetDateTime::now_utc(),
            },
        ];

        let requests = requests_for_rows(&rows);
        assert_eq!(requests.len(), 2);
    }
}
