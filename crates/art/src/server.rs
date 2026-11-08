//! Loopback HTTP server that serves cached WebP images.
//!
//! The server binds to `127.0.0.1:0` (random port) on the app's tokio runtime,
//! generates a random per-session token, and serves files from the art cache
//! only at `/<token>/<cache-key>.webp`. The token stops other local processes
//! (and any page the webview might load) from reading the cache by guessing
//! paths. `Cache-Control: immutable` tells the webview it may keep the bytes
//! for the lifetime of the process.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rand::Rng;

use crate::cache::ArtCache;
use crate::{Error, Result};

/// A running loopback server.
#[derive(Debug)]
pub struct ArtServer {
    pub port: u16,
    pub token: String,
    /// Aborted when the server handle is dropped (window closes, tests end).
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for ArtServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Clone)]
struct ServerState {
    cache: Arc<ArtCache>,
    token: Arc<str>,
}

impl ArtServer {
    /// Bind a random loopback port and start serving in the background.
    ///
    /// Must be called from inside a tokio runtime (the Dioxus desktop runtime
    /// is one).
    pub async fn start(cache: ArtCache) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|source| Error::Bind {
                addr: "127.0.0.1:0".into(),
                source: Box::new(source),
            })?;
        let port = listener
            .local_addr()
            .map_err(|source| Error::Bind {
                addr: "127.0.0.1:0".into(),
                source: Box::new(source),
            })?
            .port();

        let token: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();

        let state = ServerState {
            cache: Arc::new(cache),
            token: Arc::from(token.as_str()),
        };

        let app = Router::new()
            .route("/{token}/{file}", get(serve_file))
            .with_state(state);

        let handle = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("[pitagames-art] loopback server stopped: {error}");
            }
        });

        Ok(Self {
            port,
            token,
            handle,
        })
    }

    /// `http://127.0.0.1:{port}/{token}/{key}.webp`
    pub fn image_url(&self, cache: &ArtCache, url: &str, size: crate::ArtSize) -> String {
        format!(
            "http://127.0.0.1:{}/{}/{}.webp",
            self.port,
            self.token,
            cache.key_for(url, size)
        )
    }

    /// Same as [`image_url`] but returns `None` when the URL is absent (no
    /// store art for this game).
    pub fn image_url_opt(
        &self,
        cache: &ArtCache,
        url: Option<&str>,
        size: crate::ArtSize,
    ) -> Option<String> {
        url.map(|url| self.image_url(cache, url, size))
    }
}

/// Serve one cache file, refusing anything outside `/<token>/`.
async fn serve_file(
    State(state): State<ServerState>,
    Path((token, file)): Path<(String, String)>,
) -> Response {
    // The token check is the only access control: the port is on loopback, but
    // local processes can connect, so an unguessable path is what keeps the
    // cache private to this session.
    if token != state.token.as_ref() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Reject anything that is not a single cache file name. axum decodes
    // percent-encoding before this handler, so `%2e%2e%2f` arrives as `../`.
    if file.is_empty() || file.contains('/') || file.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }

    match tokio::fs::read(state.cache.dir().join(&file)).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "image/webp"),
                (header::CACHE_CONTROL, "public, immutable, max-age=31536000"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn start_with_file(
        name: &str,
        body: &'static [u8],
    ) -> (ArtServer, ArtCache, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().to_path_buf());
        cache.ensure_dir().unwrap();
        cache.write(name, crate::ArtSize::Grid, body).unwrap();

        let server = ArtServer::start(cache.clone()).await.unwrap();
        let key = cache.key_for(name, crate::ArtSize::Grid);
        (server, cache, key, dir)
    }

    fn url(server: &ArtServer, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", server.port)
    }

    #[tokio::test]
    async fn serves_a_cached_file_and_rejects_bad_requests() {
        let (server, _cache, key, _dir) =
            start_with_file("https://example.test/capsule.jpg", b"fake-webp").await;

        let response = reqwest::get(url(&server, &format!("/{}/{key}.webp", server.token)))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/webp"
        );
        assert!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("immutable")
        );
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"fake-webp");

        // Wrong token: 404, and the body never leaks the file.
        let response = reqwest::get(url(&server, &format!("/wrong-token/{key}.webp")))
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert!(response.bytes().await.unwrap().is_empty());

        // Missing file under the right token: 404.
        let response = reqwest::get(url(
            &server,
            &format!("/{}/does-not-exist-grid.webp", server.token),
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), 404);

        // Percent-encoded traversal is decoded by axum and then rejected.
        let response = reqwest::get(url(
            &server,
            &format!("/{}/%2e%2e%2fCargo.toml", server.token),
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn two_servers_have_different_tokens_and_ports() {
        let (first, cache, key, _first_dir) = start_with_file("https://x/a.jpg", b"a").await;
        let (second, _, _, _second_dir) = start_with_file("https://x/b.jpg", b"b").await;

        assert_ne!(first.token, second.token);
        assert_ne!(first.port, second.port);

        // The second server's token must not unlock the first server's file.
        let response = reqwest::get(url(&first, &format!("/{}/{key}.webp", second.token)))
            .await
            .unwrap();
        assert_eq!(response.status(), 404);

        // And the right token does.
        let response = reqwest::get(url(&first, &format!("/{}/{key}.webp", first.token)))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(cache.exists("https://x/a.jpg", crate::ArtSize::Grid));
    }

    #[test]
    fn image_url_contains_the_port_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().to_path_buf());
        let url = "https://cdn.example.com/hero.jpg";
        // A real server is not needed to test URL construction.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = ArtServer {
            port: 18080,
            token: "test-token-32-chars-long-xxxxxxx".into(),
            handle: runtime.spawn(async {}),
        };
        let img_url = server.image_url(&cache, url, crate::ArtSize::Grid);
        assert!(img_url.starts_with("http://127.0.0.1:18080/"));
        assert!(img_url.contains("test-token-32-chars-long-xxxxxxx"));
        assert!(img_url.ends_with(".webp"));
        assert_eq!(
            img_url,
            server.image_url(&cache, url, crate::ArtSize::Grid),
            "URL construction is deterministic"
        );
    }

    #[test]
    fn image_url_opt_is_none_for_missing_url() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtCache::new(dir.path().to_path_buf());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = ArtServer {
            port: 1,
            token: "t".into(),
            handle: runtime.spawn(async {}),
        };
        assert!(
            server
                .image_url_opt(&cache, None, crate::ArtSize::Grid)
                .is_none()
        );
        assert!(
            server
                .image_url_opt(&cache, Some("https://x"), crate::ArtSize::Grid)
                .is_some()
        );
    }
}
