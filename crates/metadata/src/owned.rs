//! Steam's `IPlayerService/GetOwnedGames` endpoint: the games an account owns.
//!
//! Unlike `appdetails`, this needs credentials: a Steam Web API key
//! (`steamcommunity.com/dev/apikey`) and the account's SteamID64. The key is
//! supplied by `pitagames-secrets`; this module never stores it.
//!
//! Two deliberate choices:
//!
//! - The key travels in the query string (Steam's documented shape), and every
//!   error message is scrubbed of it before it leaves this module. The
//!   reachable URLs and `reqwest`'s own error text both echo the request URL.
//! - One request returns the whole library, so the appdetails budget (200
//!   requests / 5 minutes) does not apply here. A refresh is a single call.
//!
//! The response shape:
//!
//! ```text
//! { "response": { "game_count": 2, "games": [
//!     { "appid": 620, "name": "Portal 2", "playtime_forever": 412,
//!       "rtime_last_played": 1700000000 } ] } }
//! ```
//!
//! A private profile or a key that belongs to another account yields
//! `{"response":{}}`, which is reported as [`OwnedError::PrivateProfile`]
//! rather than "you own nothing".

use std::time::Duration;

use pitagames_core::{GameId, OwnedGame, Store};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::transport::{ReqwestTransport, Transport};

pub const DEFAULT_ENDPOINT: &str = "https://api.steampowered.com/IPlayerService/GetOwnedGames/v1/";

/// The account's library as Steam reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedLibrary {
    /// Sorted case-insensitively by title.
    pub games: Vec<OwnedGame>,
    /// What Steam says the account owns; may exceed `games.len()` when the
    /// response is filtered by Steam itself.
    pub game_count: u32,
}

/// A `GetOwnedGames` client.
#[derive(Debug, Clone)]
pub struct SteamOwnedGames<T = ReqwestTransport> {
    transport: T,
    endpoint: String,
}

impl SteamOwnedGames<ReqwestTransport> {
    pub fn new() -> crate::Result<Self> {
        Ok(Self::with_transport(ReqwestTransport::new()?))
    }
}

impl<T: Transport> SteamOwnedGames<T> {
    pub fn with_transport(transport: T) -> Self {
        Self {
            transport,
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }

    /// Override the endpoint; tests point this at a fake transport.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Build the request URL. Public so the CLI can print it for diagnostics
    /// *without* the key (see [`request_url`] callers).
    pub fn url(&self, api_key: &str, steam_id: &str) -> String {
        format!(
            "{}?key={}&steamid={}&include_appinfo=1&include_played_free_games=1&format=json",
            self.endpoint, api_key, steam_id
        )
    }

    /// Fetch the owned library.
    ///
    /// `steam_id` is a SteamID64 (17 digits). The key is only used to sign this
    /// request and is scrubbed from every error.
    pub async fn fetch(&self, api_key: &str, steam_id: &str) -> Result<OwnedLibrary, OwnedError> {
        if steam_id.is_empty()
            || steam_id.len() > 20
            || !steam_id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(OwnedError::InvalidSteamId(steam_id.to_string()));
        }

        let url = self.url(api_key, steam_id);
        let body = match self.transport.get_text(&url).await {
            Ok(body) => body,
            Err(error) => return Err(classify(error, api_key)),
        };

        let payload: Envelope = serde_json::from_str(&body)?;
        let Some(response) = payload.response else {
            return Err(OwnedError::PrivateProfile);
        };
        if response.game_count.is_none() && response.games.is_none() {
            return Err(OwnedError::PrivateProfile);
        }

        let mut games: Vec<OwnedGame> = response
            .games
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.appid != 0)
            .map(|entry| {
                let mut game = OwnedGame::new(
                    GameId::new(Store::Steam, entry.appid.to_string()),
                    entry
                        .name
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| format!("appid {}", entry.appid)),
                );
                game.playtime =
                    Duration::from_secs(u64::from(entry.playtime_forever.unwrap_or(0)) * 60);
                game.last_played = entry
                    .rtime_last_played
                    .filter(|seconds| *seconds > 0)
                    .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok());
                game
            })
            .collect();

        games.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.id.store_id.cmp(&right.id.store_id))
        });

        let game_count = response
            .game_count
            .unwrap_or_else(|| u32::try_from(games.len()).unwrap_or(u32::MAX));

        Ok(OwnedLibrary { games, game_count })
    }
}

/// Turn a transport failure into an owned-games error with the key removed.
fn classify(error: crate::Error, api_key: &str) -> OwnedError {
    let message = error.to_string().replace(api_key, "<redacted>");
    if let crate::Error::Status { status, .. } = &error {
        return match status {
            401 | 403 => OwnedError::Rejected { status: *status },
            429 => OwnedError::RateLimited,
            _ => OwnedError::Status {
                status: *status,
                message,
            },
        };
    }
    OwnedError::Transport { message }
}

/// Failures from the owned-games endpoint.
#[derive(Debug, thiserror::Error)]
pub enum OwnedError {
    /// HTTP 401/403: a wrong, revoked or mis-scoped key.
    #[error("Steam rejected the API key (HTTP {status}); check that it belongs to this account")]
    Rejected { status: u16 },

    #[error("Steam rate-limited the owned-games request; try again later")]
    RateLimited,

    #[error("HTTP {status} from the Steam Web API: {message}")]
    Status { status: u16, message: String },

    #[error("the Steam Web API request failed: {message}")]
    Transport { message: String },

    #[error("could not read the owned-games response: {0}")]
    Decode(#[from] serde_json::Error),

    #[error(
        "Steam returned no games; the profile is private or the key belongs to another account"
    )]
    PrivateProfile,

    #[error("{0:?} is not a SteamID64")]
    InvalidSteamId(String),
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    response: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default)]
    game_count: Option<u32>,
    #[serde(default)]
    games: Option<Vec<Entry>>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    appid: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    playtime_forever: Option<u32>,
    #[serde(default)]
    rtime_last_played: Option<i64>,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::Error;

    const KEY: &str = "0123456789ABCDEF0123456789ABCDEF";
    const STEAM_ID: &str = "76561197960287930";

    #[derive(Debug)]
    struct FakeTransport {
        body: Result<String, Error>,
        urls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeTransport {
        fn ok(body: &str) -> Self {
            Self {
                body: Ok(body.to_string()),
                urls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn fail(error: Error) -> Self {
            Self {
                body: Err(error),
                urls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn recorder(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.urls)
        }
    }

    impl Transport for FakeTransport {
        async fn get_text(&self, url: &str) -> crate::Result<String> {
            self.urls.lock().unwrap().push(url.to_string());
            match &self.body {
                Ok(body) => Ok(body.clone()),
                Err(error) => Err(clone_error(error)),
            }
        }
    }

    /// `Error` is not `Clone` (it wraps `reqwest::Error`), so tests only need
    /// the two variants the owned client classifies.
    fn clone_error(error: &Error) -> Error {
        match error {
            Error::Status { status, url } => Error::Status {
                status: *status,
                url: url.clone(),
            },
            other => Error::Status {
                status: 599,
                url: other.to_string(),
            },
        }
    }

    fn client(transport: FakeTransport) -> SteamOwnedGames<FakeTransport> {
        SteamOwnedGames::with_transport(transport).endpoint("https://example.test/owned")
    }

    const LIBRARY: &str = r#"{
        "response": {
            "game_count": 3,
            "games": [
                {"appid": 620, "name": "Portal 2", "playtime_forever": 412, "rtime_last_played": 1700000000},
                {"appid": 570, "name": "Dota 2", "playtime_forever": 0, "rtime_last_played": 0},
                {"appid": 400, "name": "", "playtime_forever": 5}
            ]
        }
    }"#;

    #[tokio::test]
    async fn parses_the_library_and_keeps_steam_units() {
        let transport = FakeTransport::ok(LIBRARY);
        let urls = transport.recorder();
        let library = client(transport).fetch(KEY, STEAM_ID).await.unwrap();

        assert_eq!(library.game_count, 3);
        assert_eq!(
            library
                .games
                .iter()
                .map(|game| game.title.as_str())
                .collect::<Vec<_>>(),
            ["appid 400", "Dota 2", "Portal 2"],
            "sorted by title; a nameless entry keeps its appid"
        );

        let portal = &library.games[2];
        assert_eq!(portal.id.store_id, "620");
        assert_eq!(portal.playtime, Duration::from_secs(412 * 60));
        assert_eq!(
            portal.last_played.map(|played| played.unix_timestamp()),
            Some(1_700_000_000)
        );

        assert!(!library.games[0].playtime.is_zero());
        assert_eq!(
            library.games[1].last_played, None,
            "rtime_last_played 0 means never played"
        );

        let url = &urls.lock().unwrap()[0];
        assert!(url.starts_with("https://example.test/owned?key="), "{url}");
        assert!(url.contains(&format!("steamid={STEAM_ID}")), "{url}");
        assert!(url.contains("include_appinfo=1"));
        assert!(url.contains("include_played_free_games=1"));
        assert!(url.ends_with("format=json"), "{url}");
    }

    #[tokio::test]
    async fn an_empty_response_is_a_private_profile_not_an_empty_library() {
        let error = client(FakeTransport::ok(r#"{"response":{}}"#))
            .fetch(KEY, STEAM_ID)
            .await
            .unwrap_err();
        assert!(matches!(error, OwnedError::PrivateProfile));

        let error = client(FakeTransport::ok("{}"))
            .fetch(KEY, STEAM_ID)
            .await
            .unwrap_err();
        assert!(matches!(error, OwnedError::PrivateProfile));
    }

    #[tokio::test]
    async fn an_account_with_no_games_is_a_valid_empty_library() {
        let library = client(FakeTransport::ok(
            r#"{"response":{"game_count":0,"games":[]}}"#,
        ))
        .fetch(KEY, STEAM_ID)
        .await
        .unwrap();
        assert!(library.games.is_empty());
        assert_eq!(library.game_count, 0);
    }

    #[tokio::test]
    async fn a_rejected_key_is_recognised_and_never_echoed() {
        let transport = FakeTransport::fail(Error::Status {
            status: 403,
            url: format!("https://api.steampowered.com/IPlayerService/GetOwnedGames/v1/?key={KEY}"),
        });
        let error = client(transport).fetch(KEY, STEAM_ID).await.unwrap_err();

        assert!(matches!(error, OwnedError::Rejected { status: 403 }));
        assert!(!error.to_string().contains(KEY));

        let transport = FakeTransport::fail(Error::Status {
            status: 429,
            url: String::new(),
        });
        let error = client(transport).fetch(KEY, STEAM_ID).await.unwrap_err();
        assert!(matches!(error, OwnedError::RateLimited));
    }

    #[tokio::test]
    async fn transport_errors_are_scrubbed_of_the_key() {
        let transport = FakeTransport::fail(Error::Status {
            status: 500,
            url: format!("https://api.steampowered.com/IPlayerService/GetOwnedGames/v1/?key={KEY}"),
        });
        let error = client(transport).fetch(KEY, STEAM_ID).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("HTTP 500"), "{message}");
        assert!(message.contains("<redacted>"), "{message}");
        assert!(!message.contains(KEY), "{message}");
    }

    #[tokio::test]
    async fn a_non_numeric_steam_id_fails_before_the_request() {
        let transport = FakeTransport::ok("{}");
        let urls = transport.recorder();
        let error = client(transport)
            .fetch(KEY, "not-a-steamid")
            .await
            .unwrap_err();
        assert!(matches!(error, OwnedError::InvalidSteamId(_)));
        assert!(urls.lock().unwrap().is_empty());
    }
}
