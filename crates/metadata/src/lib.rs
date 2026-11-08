//! Store metadata: fetching, parsing, rate limiting and cache policy.
//!
//! This crate is framework-free and unaware of SQLite. It knows how to talk to
//! Steam's `appdetails` endpoint, how to turn the response into
//! [`pitagames_core::GameMetadata`], and how long a cached answer stays usable.
//! `pitagames-db` stores those values and `crates/app` decides when to refresh.
//!
//! One appid per request: Steam's `appdetails` endpoint returns `null` for
//! comma-separated batches (verified 2026-11), so the 200-requests-per-five-
//! minutes budget in [`RateLimit::steam_app_details`] is the real gate for a
//! large library.

pub mod rate;
pub mod steam;
pub mod text;
pub mod transport;

use std::time::Duration;

use time::OffsetDateTime;

pub use rate::{RateLimit, TokenBucket};
pub use steam::{
    AppFailure, DEFAULT_FILTERS, DEFAULT_LANGUAGE, FailureReason, Fetched, SteamAppDetails,
};
pub use text::unescape_html;
pub use transport::{ReqwestTransport, Transport};

/// The ready-to-use client: reqwest over rustls.
pub type SteamClient = SteamAppDetails<ReqwestTransport>;

/// How long a metadata answer may be reused before it is fetched again.
///
/// Successful and negative answers have different lifetimes: a game that exists
/// today is unlikely to disappear, while a store hiccup should not be cached for
/// a week. Transport failures are never cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    /// Reuse a positive answer for this long.
    pub found_ttl: Duration,
    /// Reuse "the store has no data" for this long.
    pub missing_ttl: Duration,
}

impl CachePolicy {
    pub const DEFAULT: Self = Self {
        found_ttl: Duration::from_secs(7 * 24 * 60 * 60),
        missing_ttl: Duration::from_secs(24 * 60 * 60),
    };

    /// Whether a cached row may be shown without fetching again.
    ///
    /// A future `fetched_at` (clock moved backwards) counts as fresh: trusting
    /// the stored value is safer than a fetch storm.
    pub fn is_fresh(&self, fetched_at: OffsetDateTime, found: bool, now: OffsetDateTime) -> bool {
        let ttl = if found {
            self.found_ttl
        } else {
            self.missing_ttl
        };
        let age = (now - fetched_at).whole_seconds();
        // Negative age means the clock moved backwards; treat it as fresh.
        age < ttl.as_secs() as i64
    }
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Metadata failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("HTTP {status} from {url}")]
    Status { status: u16, url: String },

    #[error("could not build the HTTP client: {0}")]
    Client(#[source] reqwest::Error),

    #[error("could not decode the app details response: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("no app ids were requested")]
    NoAppIds,
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    #[test]
    fn found_metadata_is_fresh_for_a_week() {
        let policy = CachePolicy::DEFAULT;
        let fetched = at(1_000_000);
        assert!(policy.is_fresh(fetched, true, fetched));
        assert!(policy.is_fresh(fetched, true, fetched + time::Duration::days(6)));
        assert!(!policy.is_fresh(fetched, true, fetched + time::Duration::days(7)));
    }

    #[test]
    fn missing_metadata_is_retried_the_next_day() {
        let policy = CachePolicy::DEFAULT;
        let fetched = at(1_000_000);
        assert!(policy.is_fresh(fetched, false, fetched + time::Duration::hours(23)));
        assert!(!policy.is_fresh(fetched, false, fetched + time::Duration::hours(24)));
    }

    #[test]
    fn a_backwards_clock_does_not_force_a_refetch() {
        let policy = CachePolicy::DEFAULT;
        let fetched = at(1_000_000);
        assert!(policy.is_fresh(fetched, true, fetched - time::Duration::days(30)));
    }
}
