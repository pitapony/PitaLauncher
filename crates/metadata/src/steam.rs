//! Steam's `appdetails` endpoint: request shape, parsing and batching.
//!
//! The endpoint answers one appid per request (a comma-separated list returns
//! `null`), so [`SteamAppDetails::fetch`] walks the ids one by one, taking a
//! token from the bucket before each request.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use pitagames_core::{AppKind, GameId, GameMetadata, Platforms, Store};
use serde::Deserialize;

use crate::rate::{RateLimit, TokenBucket};
use crate::text::unescape_html;
use crate::transport::{ReqwestTransport, Transport};
use crate::{Error, Result};

pub const DEFAULT_ENDPOINT: &str = "https://store.steampowered.com/api/appdetails";
pub const DEFAULT_LANGUAGE: &str = "english";

/// Keep the payload to the fields the UI reads.
///
/// `basic` includes the description, name, type and the header/capsule art; the
/// rest are the one-line facts a detail page shows. Screenshots and movies are
/// deliberately excluded: the art pipeline owns images, and videos are out of
/// scope for v1.
pub const DEFAULT_FILTERS: &str =
    "basic,developers,publishers,genres,release_date,platforms,metacritic,background_raw";

/// Why one appid has no metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// The endpoint answered `success: false` (delisted, region-locked, …).
    NoData,
    /// The response did not contain the appid we asked for.
    Missing,
    /// `success: true` but the payload was unusable.
    Incomplete,
}

impl FailureReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoData => "the store has no data for this app",
            Self::Missing => "the app was missing from the response",
            Self::Incomplete => "the response was incomplete",
        }
    }
}

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One appid the store could not describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppFailure {
    pub app_id: u32,
    pub reason: FailureReason,
}

/// The outcome of a fetch: what we learned and what we did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    pub items: Vec<GameMetadata>,
    pub failures: Vec<AppFailure>,
}

impl Fetched {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.failures.is_empty()
    }
}

/// A rate-limited `appdetails` client.
#[derive(Debug, Clone)]
pub struct SteamAppDetails<T = ReqwestTransport> {
    transport: T,
    endpoint: String,
    language: String,
    filters: String,
    bucket: TokenBucket,
}

impl SteamAppDetails<ReqwestTransport> {
    /// Build a client with rustls and a 30 s request timeout.
    pub fn new() -> Result<Self> {
        Ok(Self::with_transport(ReqwestTransport::new()?))
    }
}

impl<T: Transport> SteamAppDetails<T> {
    pub fn with_transport(transport: T) -> Self {
        Self {
            transport,
            endpoint: DEFAULT_ENDPOINT.to_string(),
            language: DEFAULT_LANGUAGE.to_string(),
            filters: DEFAULT_FILTERS.to_string(),
            bucket: TokenBucket::new(RateLimit::steam_app_details(), Instant::now()),
        }
    }

    /// Override the endpoint; tests point this at a fake transport.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Steam language code, e.g. `english`, `german`, `schinese`.
    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    /// Override the requested fields; the default is [`DEFAULT_FILTERS`].
    pub fn filters(mut self, filters: impl Into<String>) -> Self {
        self.filters = filters.into();
        self
    }

    /// Replace the budget, for tests and for callers that already hold a token.
    pub fn rate_limit(mut self, limit: RateLimit) -> Self {
        self.bucket = TokenBucket::new(limit, Instant::now());
        self
    }

    pub fn bucket(&self) -> &TokenBucket {
        &self.bucket
    }

    /// Fetch metadata for every id, deduplicating and rate-limiting requests.
    ///
    /// A transport failure aborts the fetch: the caller keeps whatever was in
    /// the cache and retries later. Per-app failures are reported in
    /// [`Fetched::failures`] so they can be cached as "no data".
    pub async fn fetch(&mut self, app_ids: &[u32]) -> Result<Fetched> {
        let mut ids = Vec::with_capacity(app_ids.len());
        for app_id in app_ids {
            if !ids.contains(app_id) {
                ids.push(*app_id);
            }
        }
        if ids.is_empty() {
            return Err(Error::NoAppIds);
        }

        let mut fetched = Fetched::default();
        for app_id in ids {
            self.acquire().await;
            let url = self.url(app_id);
            let body = self.transport.get_text(&url).await?;
            let one = parse_app_details(&body, &[app_id])?;
            fetched.items.extend(one.items);
            fetched.failures.extend(one.failures);
        }

        Ok(fetched)
    }

    fn url(&self, app_id: u32) -> String {
        format!(
            "{}?appids={app_id}&l={}&filters={}",
            self.endpoint, self.language, self.filters
        )
    }

    async fn acquire(&mut self) {
        loop {
            match self.bucket.acquire(Instant::now()) {
                None => return,
                Some(delay) => tokio::time::sleep(delay.max(Duration::from_millis(1))).await,
            }
        }
    }
}

/// Parse one `appdetails` response body for the requested ids.
///
/// `null` is a real response (Steam returns it for an unusable query), so it
/// becomes a per-app "no data" failure rather than a decode error.
pub fn parse_app_details(body: &str, requested: &[u32]) -> Result<Fetched> {
    let value: serde_json::Value = serde_json::from_str(body)?;

    if value.is_null() {
        return Ok(Fetched {
            items: Vec::new(),
            failures: requested
                .iter()
                .map(|app_id| AppFailure {
                    app_id: *app_id,
                    reason: FailureReason::NoData,
                })
                .collect(),
        });
    }

    let entries: BTreeMap<String, Entry> = serde_json::from_value(value)?;
    let mut fetched = Fetched::default();

    for app_id in requested {
        let Some(entry) = entries.get(&app_id.to_string()) else {
            fetched.failures.push(AppFailure {
                app_id: *app_id,
                reason: FailureReason::Missing,
            });
            continue;
        };

        if !entry.success {
            fetched.failures.push(AppFailure {
                app_id: *app_id,
                reason: FailureReason::NoData,
            });
            continue;
        }

        let Some(payload) = entry.data.as_ref() else {
            fetched.failures.push(AppFailure {
                app_id: *app_id,
                reason: FailureReason::Incomplete,
            });
            continue;
        };

        match payload.to_metadata(*app_id) {
            Ok(metadata) => fetched.items.push(metadata),
            Err(reason) => fetched.failures.push(AppFailure {
                app_id: *app_id,
                reason,
            }),
        }
    }

    Ok(fetched)
}

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    developers: Option<Vec<String>>,
    #[serde(default)]
    publishers: Option<Vec<String>>,
    #[serde(default)]
    genres: Option<Vec<Genre>>,
    #[serde(default)]
    release_date: Option<ReleaseDate>,
    #[serde(default)]
    platforms: Option<Platforms>,
    #[serde(default)]
    metacritic: Option<Metacritic>,
    #[serde(default)]
    header_image: Option<String>,
    #[serde(default)]
    capsule_image: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    background_raw: Option<String>,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    is_free: bool,
    #[serde(default)]
    controller_support: Option<String>,
}

impl Payload {
    fn to_metadata(&self, app_id: u32) -> std::result::Result<GameMetadata, FailureReason> {
        let name = self
            .name
            .as_deref()
            .map(unescape_html)
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .ok_or(FailureReason::Incomplete)?;

        let mut metadata = GameMetadata::new(GameId::new(Store::Steam, app_id.to_string()), name);
        metadata.kind = AppKind::from(self.kind.as_deref().unwrap_or("game"));
        metadata.short_description = self
            .short_description
            .as_deref()
            .map(unescape_html)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        metadata.developers = strings(&self.developers);
        metadata.publishers = strings(&self.publishers);
        metadata.genres = self
            .genres
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|genre| unescape_html(&genre.description).trim().to_string())
            .filter(|genre| !genre.is_empty())
            .collect();
        if let Some(release) = &self.release_date {
            metadata.release_date = Some(unescape_html(&release.date).trim().to_string());
            metadata.coming_soon = release.coming_soon;
        }
        metadata.platforms = self.platforms.unwrap_or_default();
        metadata.metacritic = self.metacritic.as_ref().map(|score| score.score);
        metadata.header_image = self.header_image.clone();
        metadata.capsule_image = self.capsule_image.clone();
        metadata.background = self
            .background_raw
            .clone()
            .or_else(|| self.background.clone());
        metadata.website = self.website.clone();
        metadata.is_free = self.is_free;
        metadata.controller_support = self.controller_support.clone();

        Ok(metadata)
    }
}

fn strings(values: &Option<Vec<String>>) -> Vec<String> {
    values
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|value| unescape_html(value).trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

#[derive(Debug, Deserialize)]
struct Genre {
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseDate {
    #[serde(default)]
    coming_soon: bool,
    #[serde(default)]
    date: String,
}

#[derive(Debug, Deserialize)]
struct Metacritic {
    score: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const PORTAL_2: &str = include_str!("../../../fixtures/steam/appdetails_620.json");
    const MISSING: &str = include_str!("../../../fixtures/steam/appdetails_missing.json");

    fn portal_2() -> GameMetadata {
        parse_app_details(PORTAL_2, &[620]).unwrap().items.remove(0)
    }

    #[test]
    fn parses_the_fields_the_detail_page_shows() {
        let metadata = portal_2();

        assert_eq!(metadata.game, GameId::new(Store::Steam, "620"));
        assert_eq!(metadata.name, "Portal 2");
        assert_eq!(metadata.kind, AppKind::Game);
        assert_eq!(metadata.developers, ["Valve"]);
        assert_eq!(metadata.publishers, ["Valve"]);
        assert_eq!(metadata.genres, ["Action", "Adventure"]);
        assert_eq!(metadata.release_date.as_deref(), Some("Apr 18, 2011"));
        assert!(!metadata.coming_soon);
        assert_eq!(
            metadata.platforms,
            Platforms {
                windows: true,
                mac: false,
                linux: true
            }
        );
        assert_eq!(metadata.metacritic, Some(95));
        assert!(!metadata.is_free);
        assert_eq!(metadata.controller_support.as_deref(), Some("full"));
        assert!(
            metadata
                .header_image
                .as_deref()
                .is_some_and(|url| url.starts_with("https://")),
            "header art is kept as a URL for the art pipeline"
        );
    }

    #[test]
    fn decodes_html_entities_in_store_text() {
        let metadata = portal_2();
        let description = metadata.short_description.unwrap();
        assert!(
            description.starts_with("The \"Perpetual Testing Initiative\""),
            "{description}"
        );
        assert!(!description.contains("&quot;"), "{description}");
    }

    #[test]
    fn a_missing_app_is_a_failure_not_an_error() {
        let fetched = parse_app_details(MISSING, &[999_999_999]).unwrap();
        assert!(fetched.items.is_empty());
        assert_eq!(
            fetched.failures,
            [AppFailure {
                app_id: 999_999_999,
                reason: FailureReason::NoData
            }]
        );
    }

    #[test]
    fn null_and_absent_ids_are_reported_per_app() {
        let fetched = parse_app_details("null", &[620, 440]).unwrap();
        assert_eq!(fetched.failures.len(), 2);
        assert!(
            fetched
                .failures
                .iter()
                .all(|failure| failure.reason == FailureReason::NoData)
        );

        let fetched = parse_app_details(PORTAL_2, &[620, 440]).unwrap();
        assert_eq!(fetched.items.len(), 1);
        assert_eq!(
            fetched.failures,
            [AppFailure {
                app_id: 440,
                reason: FailureReason::Missing
            }]
        );
    }

    #[test]
    fn success_without_data_is_incomplete() {
        let fetched = parse_app_details(r#"{"620": {"success": true}}"#, &[620]).unwrap();
        assert!(fetched.items.is_empty());
        assert_eq!(fetched.failures[0].reason, FailureReason::Incomplete);
    }

    #[test]
    fn a_payload_without_a_name_is_incomplete() {
        let fetched = parse_app_details(
            r#"{"620": {"success": true, "data": {"type": "game"}}}"#,
            &[620],
        )
        .unwrap();
        assert!(fetched.items.is_empty());
        assert_eq!(fetched.failures[0].reason, FailureReason::Incomplete);
    }

    #[test]
    fn malformed_json_is_an_error() {
        let error = parse_app_details("<html>not json</html>", &[620]).unwrap_err();
        assert!(matches!(error, Error::Decode(_)), "{error}");
    }

    #[derive(Default)]
    struct FakeTransport {
        bodies: Mutex<Vec<(String, String)>>,
    }

    impl FakeTransport {
        fn push(&self, url_fragment: &str, body: &str) {
            self.bodies
                .lock()
                .unwrap()
                .push((url_fragment.to_string(), body.to_string()));
        }
    }

    impl Transport for FakeTransport {
        async fn get_text(&self, url: &str) -> Result<String> {
            let mut bodies = self.bodies.lock().unwrap();
            let index = bodies
                .iter()
                .position(|(fragment, _)| url.contains(fragment.as_str()))
                .expect("unexpected URL");
            Ok(bodies.remove(index).1)
        }
    }

    #[tokio::test]
    async fn fetch_requests_one_appid_at_a_time_and_deduplicates() {
        let transport = FakeTransport::default();
        transport.push("appids=620", PORTAL_2);
        transport.push("appids=999999999", MISSING);

        let mut client = SteamAppDetails::with_transport(transport)
            .rate_limit(RateLimit::new(100, Duration::from_secs(1)));
        let fetched = client.fetch(&[620, 999_999_999, 620]).await.unwrap();

        assert_eq!(fetched.items.len(), 1);
        assert_eq!(fetched.items[0].game.store_id, "620");
        assert_eq!(fetched.failures.len(), 1);
    }

    #[tokio::test]
    async fn an_empty_fetch_is_an_error() {
        let mut client = SteamAppDetails::with_transport(FakeTransport::default());
        assert!(matches!(client.fetch(&[]).await, Err(Error::NoAppIds)));
    }

    #[test]
    fn urls_carry_the_language_and_filters() {
        let client = SteamAppDetails::with_transport(FakeTransport::default())
            .endpoint("https://example.test/appdetails")
            .language("german");
        let url = client.url(620);
        assert_eq!(
            url,
            format!(
                "https://example.test/appdetails?appids=620&l=german&filters={DEFAULT_FILTERS}"
            )
        );
    }
}
